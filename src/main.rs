//! linkshrink: go/ links for wupinyin.co.uk. Originally by d0nut and goproslowyo (2022-23, axum + Redis);
//! rewritten for kvm-vps in 2026 (SQLite, the Kanidm gate, OCI app) with the same idea at its core: every link in
//! memory, redirects never wait on storage.
//!
//!   linkshrink              serve (settings: see config.rs)
//!   linkshrink healthcheck  GET /healthz on LINKSHRINK_LISTEN, exit 0 when it answers 200 (deploys, fire-drill)

mod auth;
mod config;
mod metrics;
mod secretfile;
mod store;
mod web;

use std::{io::{Read, Write},
          net::{Ipv4Addr, SocketAddr, TcpStream},
          process::ExitCode,
          sync::{Arc, atomic::Ordering},
          time::Duration};

use tracing::{error, info, warn};

use crate::{auth::Auth, config::Config, store::Store, web::AppState};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// How often a consistent copy of the database is written for the backups, when something changed.
const SNAPSHOT_EVERY: Duration = Duration::from_secs(300);

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }
    tracing_subscriber::fmt().with_ansi(false)
                             .with_target(false)
                             .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("LINKSHRINK_LOG")
                                 .unwrap_or_else(|_| "info".into()))
                             .init();
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            error!("config: {e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            error!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match rt.block_on(serve(cfg)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn serve(cfg: Config) -> Result<(), String> {
    let store = Arc::new(Store::open(&cfg.db).map_err(|e| format!("opening {}: {e}", cfg.db.display()))?);
    let secret = secretfile::SecretFile::open(&cfg.gate_secret_file, Duration::from_secs(1))?;
    let auth = Arc::new(Auth::new(cfg.trusted_proxy, secret, cfg.user_group.clone(), cfg.admin_group.clone()));
    let state = AppState { store: store.clone(), auth, public_origin: cfg.public_origin.clone().into() };

    let listener = tokio::net::TcpListener::bind(cfg.listen).await.map_err(|e| format!("listen {}: {e}", cfg.listen))?;
    info!(listen = %cfg.listen, trusted_proxy = %cfg.trusted_proxy, user_group = ?cfg.user_group,
          admin_group = %cfg.admin_group, origin = %cfg.public_origin, "linkshrink listening");

    let background = tokio::spawn(background(store.clone(), cfg.flush_every));
    let app = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let served = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await;

    background.abort();
    final_save(&store).await;
    served.map_err(|e| format!("server: {e}"))
}

/// Hit counts every `flush_every`; a backup snapshot at most every SNAPSHOT_EVERY when anything changed.
async fn background(store: Arc<Store>, flush_every: Duration) {
    let mut tick = tokio::time::interval(flush_every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_snapshot = tokio::time::Instant::now();
    let mut snapshot_generation = u64::MAX; // forces a first snapshot
    let mut hits_since_snapshot = false;
    loop {
        tick.tick().await;
        match store.flush_hits().await {
            Ok(0) => {}
            Ok(_) => {
                metrics::FLUSHES.inc();
                hits_since_snapshot = true;
            }
            Err(e) => {
                metrics::FLUSH_ERRORS.inc();
                warn!("hit flush failed (will retry): {e}");
            }
        }
        let generation = store.generation.load(Ordering::Relaxed);
        let edited = generation != snapshot_generation;
        if edited || (hits_since_snapshot && last_snapshot.elapsed() >= SNAPSHOT_EVERY) {
            match store.snapshot().await {
                Ok(()) => {
                    snapshot_generation = generation;
                    hits_since_snapshot = false;
                    last_snapshot = tokio::time::Instant::now();
                }
                Err(e) => warn!("backup snapshot failed (will retry): {e}"),
            }
        }
    }
}

async fn final_save(store: &Store) {
    match store.flush_hits().await {
        Ok(n) => info!(rows = n, "hit counts saved at shutdown"),
        Err(e) => error!("hit flush at shutdown failed: {e}"),
    }
    if let Err(e) = store.snapshot().await {
        error!("backup snapshot at shutdown failed: {e}");
    }
}

/// PID 1 in the container gets no default signal handling: SIGTERM (incus stop) and SIGINT both mean "drain and
/// exit".
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        error!("can't install signal handlers; stop will kill without saving the last hits");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => info!("SIGTERM: shutting down"),
        _ = int.recv() => info!("SIGINT: shutting down"),
    }
}

/// `linkshrink healthcheck`: no HTTP client library, just HTTP/1.0 over a socket (the image has no curl).
fn healthcheck() -> ExitCode {
    let listen = std::env::var("LINKSHRINK_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let Ok(mut addr) = listen.parse::<SocketAddr>() else {
        eprintln!("healthcheck: bad LINKSHRINK_LISTEN {listen:?}");
        return ExitCode::FAILURE;
    };
    if addr.ip().is_unspecified() {
        addr.set_ip(Ipv4Addr::LOCALHOST.into());
    }
    let result = (|| -> std::io::Result<String> {
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut buf = Vec::with_capacity(256);
        s.take(4096).read_to_end(&mut buf)?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    })();
    match result {
        Ok(r) if r.starts_with("HTTP/1.1 200") || r.starts_with("HTTP/1.0 200") => ExitCode::SUCCESS,
        Ok(r) => {
            eprintln!("healthcheck: {}", r.lines().next().unwrap_or("empty answer"));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("healthcheck: {addr}: {e}");
            ExitCode::FAILURE
        }
    }
}
