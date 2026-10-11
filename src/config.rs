//! Settings, all from the environment (no secrets in argv or env: the gate secret is read from a file).

use std::{env, net::{IpAddr, SocketAddr}, path::PathBuf, time::Duration};

#[derive(Clone, Debug)]
pub struct Config {
    /// LINKSHRINK_LISTEN, default 127.0.0.1:8080.
    pub listen: SocketAddr,
    /// LINKSHRINK_DB, the SQLite file (default /var/lib/linkshrink/links.db).
    pub db: PathBuf,
    /// LINKSHRINK_TRUSTED_PROXY: the only peer whose gate headers are believed (Caddy, 10.12.0.10).
    pub trusted_proxy: IpAddr,
    /// LINKSHRINK_GATE_SECRET_FILE: Caddy sends it in X-Gate-Auth (iac #97); "current\nprevious" during a
    /// rotation, re-read when it changes (secretfile.rs).
    pub gate_secret_file: PathBuf,
    /// LINKSHRINK_USER_GROUP: when set, a signed-in person must also be in this Kanidm group.
    pub user_group: Option<String>,
    /// LINKSHRINK_ADMIN_GROUP, default admins: may edit and delete every link and sees every hidden link.
    pub admin_group: String,
    /// LINKSHRINK_PUBLIC_ORIGIN (required), e.g. https://go.example.com: the only Origin a form POST may come from, and
    /// the base of the links people copy.
    pub public_origin: String,
    /// LINKSHRINK_HELP: shown to signed-in people who aren't in LINKSHRINK_USER_GROUP (who to ask for access).
    pub help: String,
    /// LINKSHRINK_REDIRECT_CACHE_SECS, default 0: let browsers keep a redirect for this long (Cache-Control: private,
    /// max-age). Repeat visits then skip the network; an edit reaches such a browser up to this late, and its repeat
    /// visits aren't counted as hits. 0 = never cached.
    pub redirect_cache_secs: u64,
    /// LINKSHRINK_FLUSH_SECS, default 5: how often hit counts are written to the database.
    pub flush_every: Duration,
}

fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let listen = var("LINKSHRINK_LISTEN").unwrap_or_else(|| "127.0.0.1:8080".into());
        let listen = listen.parse().map_err(|e| format!("LINKSHRINK_LISTEN {listen:?}: {e}"))?;

        let trusted_proxy = var("LINKSHRINK_TRUSTED_PROXY").ok_or("LINKSHRINK_TRUSTED_PROXY is required (Caddy's address)")?;
        let trusted_proxy = trusted_proxy.parse()
                                         .map_err(|e| format!("LINKSHRINK_TRUSTED_PROXY {trusted_proxy:?}: {e}"))?;

        let secret_file = var("LINKSHRINK_GATE_SECRET_FILE").ok_or("LINKSHRINK_GATE_SECRET_FILE is required")?;

        let public_origin = var("LINKSHRINK_PUBLIC_ORIGIN").ok_or("LINKSHRINK_PUBLIC_ORIGIN is required (e.g. https://go.example.com)")?;
        let public_origin = public_origin.trim_end_matches('/').to_string();
        if !(public_origin.starts_with("https://") || public_origin.starts_with("http://")) {
            return Err(format!("LINKSHRINK_PUBLIC_ORIGIN {public_origin:?} must start with https:// or http://"));
        }
        let cache = var("LINKSHRINK_REDIRECT_CACHE_SECS").unwrap_or_else(|| "0".into());
        let redirect_cache_secs: u64 = cache.parse().map_err(|e| format!("LINKSHRINK_REDIRECT_CACHE_SECS {cache:?}: {e}"))?;

        let flush_secs = var("LINKSHRINK_FLUSH_SECS").unwrap_or_else(|| "5".into());
        let flush_secs: u64 = flush_secs.parse().map_err(|e| format!("LINKSHRINK_FLUSH_SECS {flush_secs:?}: {e}"))?;

        Ok(Self { listen,
                  db: var("LINKSHRINK_DB").unwrap_or_else(|| "/var/lib/linkshrink/links.db".into()).into(),
                  trusted_proxy,
                  gate_secret_file: secret_file.into(),
                  user_group: var("LINKSHRINK_USER_GROUP"),
                  admin_group: var("LINKSHRINK_ADMIN_GROUP").unwrap_or_else(|| "admins".into()),
                  public_origin,
                  help: var("LINKSHRINK_HELP").unwrap_or_else(|| "Ask an admin for access.".into()),
                  redirect_cache_secs,
                  flush_every: Duration::from_secs(flush_secs.max(1)) })
    }
}
