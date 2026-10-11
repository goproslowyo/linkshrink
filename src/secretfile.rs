//! The X-Gate-Auth secret file, rotated without a gap (iac #72 W4, same rules as welcome's internal/secretfile):
//! line 1 is the current secret, an optional line 2 the previous one (present only during a rotation), and both are
//! accepted. bao-render replaces the file atomically (rename), so a change shows as a new inode/mtime/size; the file
//! is checked on use, at most once per `every`, by one request at a time, and nothing else waits for it. A missing or
//! unusable file keeps the last good secrets (logged); at start it fails closed.

use std::{os::unix::fs::MetadataExt,
          path::{Path, PathBuf},
          sync::{Arc,
                 atomic::{AtomicBool, AtomicU64, Ordering}},
          time::{Duration, Instant}};

use arc_swap::ArcSwap;
use subtle::ConstantTimeEq;
use tracing::{info, warn};

const MIN_LEN: usize = 16;

#[derive(Debug, PartialEq, Eq)]
struct Loaded {
    secrets: Vec<Vec<u8>>,
    stamp: (u64, i64, i64, u64), // inode, mtime s, mtime ns, size
}

pub struct SecretFile {
    path: PathBuf,
    loaded: ArcSwap<Loaded>,
    start: Instant,
    every_ms: u64,
    next_check_ms: AtomicU64,
    previous_logged: AtomicBool,
}

fn stamp(path: &Path) -> std::io::Result<(u64, i64, i64, u64)> {
    let m = std::fs::metadata(path)?;
    Ok((m.ino(), m.mtime(), m.mtime_nsec(), m.size()))
}

/// Line 1 (current) and an optional line 2 (previous); each trimmed, at least MIN_LEN bytes. More lines = unusable.
fn parse(raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let lines: Vec<&[u8]> = raw.split(|b| *b == b'\n').map(<[u8]>::trim_ascii).filter(|l| !l.is_empty()).collect();
    match lines.len() {
        0 => Err("empty".into()),
        1 | 2 if lines.iter().all(|l| l.len() >= MIN_LEN) => Ok(lines.into_iter().map(<[u8]>::to_vec).collect()),
        1 | 2 => Err(format!("a secret shorter than {MIN_LEN} bytes")),
        n => Err(format!("{n} lines (current + previous at most)")),
    }
}

fn load(path: &Path) -> Result<Loaded, String> {
    let before = stamp(path).map_err(|e| e.to_string())?;
    let raw = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(Loaded { secrets: parse(&raw)?, stamp: before })
}

impl SecretFile {
    /// Fails (closed) when the file is missing or unusable at start.
    pub fn open(path: &Path, every: Duration) -> Result<Self, String> {
        let loaded = load(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { path: path.to_path_buf(),
                  loaded: ArcSwap::from_pointee(loaded),
                  start: Instant::now(),
                  every_ms: every.as_millis() as u64,
                  next_check_ms: AtomicU64::new(every.as_millis() as u64),
                  previous_logged: AtomicBool::new(false) })
    }

    #[cfg(test)]
    pub fn fixed(secret: &[u8]) -> Self {
        Self { path: PathBuf::new(),
               loaded: ArcSwap::from_pointee(Loaded { secrets: vec![secret.to_vec()], stamp: (0, 0, 0, 0) }),
               start: Instant::now(),
               every_ms: u64::MAX,
               next_check_ms: AtomicU64::new(u64::MAX),
               previous_logged: AtomicBool::new(false) }
    }

    fn maybe_reload(&self) {
        let now = self.start.elapsed().as_millis() as u64;
        let due = self.next_check_ms.load(Ordering::Relaxed);
        if now < due
           || self.next_check_ms
                  .compare_exchange(due, now.saturating_add(self.every_ms), Ordering::AcqRel, Ordering::Relaxed)
                  .is_err()
        {
            return; // not due, or another request is checking
        }
        let current = self.loaded.load();
        match stamp(&self.path) {
            Ok(s) if s == current.stamp => {}
            Ok(_) => match load(&self.path) {
                Ok(l) => {
                    info!(secrets = l.secrets.len(), "gate secret file changed: reloaded");
                    self.loaded.store(Arc::new(l));
                    self.previous_logged.store(false, Ordering::Relaxed);
                }
                Err(e) => warn!("gate secret file {} unusable ({e}); keeping the last good secrets", self.path.display()),
            },
            Err(e) => warn!("gate secret file {}: {e}; keeping the last good secrets", self.path.display()),
        }
    }

    /// Whether `presented` is one of the accepted secrets (constant time per candidate, every candidate compared).
    pub fn accepts(&self, presented: &[u8]) -> bool {
        self.maybe_reload();
        let loaded = self.loaded.load();
        let mut hit = subtle::Choice::from(0u8);
        let mut previous = false;
        for (i, s) in loaded.secrets.iter().enumerate() {
            let m = presented.ct_eq(s);
            previous |= i > 0 && bool::from(m);
            hit |= m;
        }
        if previous && !self.previous_logged.swap(true, Ordering::Relaxed) {
            info!("X-Gate-Auth matched the PREVIOUS gate secret (a rotation is in progress)");
        }
        bool::from(hit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(name: &str, content: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("linkshrink-secret-{name}-{}", std::process::id()));
        std::fs::write(&p, content).unwrap();
        p
    }

    /// bao-render's way: write a temp file, rename over (new inode)
    fn replace(p: &Path, content: &str) {
        let tmp = p.with_extension("new");
        std::fs::write(&tmp, content).unwrap();
        std::fs::rename(&tmp, p).unwrap();
    }

    #[test]
    fn parse_rules() {
        assert_eq!(parse(b"aaaaaaaaaaaaaaaa\n").unwrap().len(), 1);
        assert_eq!(parse(b"aaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbb\n").unwrap().len(), 2);
        assert!(parse(b"").is_err());
        assert!(parse(b"short\n").is_err());
        assert!(parse(b"aaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbb\ncccccccccccccccc").is_err());
    }

    #[test]
    fn rotation_without_a_gap() {
        let (a, b) = ("rotation-test-old-0001", "rotation-test-new-0002");
        let p = tmpfile("rotate", &format!("{a}\n"));
        let f = SecretFile::open(&p, Duration::ZERO).unwrap();
        assert!(f.accepts(a.as_bytes()) && !f.accepts(b.as_bytes()));
        // receivers first: new + old
        replace(&p, &format!("{b}\n{a}\n"));
        assert!(f.accepts(a.as_bytes()) && f.accepts(b.as_bytes()));
        // then new only: old refused
        replace(&p, &format!("{b}\n"));
        assert!(!f.accepts(a.as_bytes()) && f.accepts(b.as_bytes()));
        // an unusable file keeps the last good secrets
        replace(&p, "x\n");
        assert!(f.accepts(b.as_bytes()));
        std::fs::remove_file(&p).unwrap();
        assert!(f.accepts(b.as_bytes()));
        assert!(!f.accepts(b"") && !f.accepts(&b.as_bytes()[..10]));
    }

    #[test]
    fn fails_closed_at_start() {
        assert!(SecretFile::open(Path::new("/nonexistent/linkshrink-secret"), Duration::ZERO).is_err());
        let p = tmpfile("short", "short\n");
        assert!(SecretFile::open(&p, Duration::ZERO).is_err());
        let _ = std::fs::remove_file(p);
    }
}
