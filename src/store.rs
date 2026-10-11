//! The links. Every link lives in memory, in an immutable map behind an atomic pointer swap: a redirect is one
//! atomic load, one hash lookup and two relaxed atomic writes (hit count, last used), with no lock and no I/O. SQLite
//! is the source of truth on disk: edits are written through (database first, then a new map is swapped in), and hit
//! counts are added up in memory and written back every few seconds, only for the links that were used (d0nut's
//! original design kept the hits in its cache and flushed them to Redis the same way).
//!
//! A link's counters are shared between the old and the new map entry, so an edit never loses or reverts hits, and
//! the flush only ever writes `hits` and `last_used`, so it can't revert an edit either. An alias is a second key for
//! the SAME `Arc<Entry>`: following it costs exactly what following the link's own name does, and counts as the link.

use std::{collections::HashMap,
          fmt,
          hash::{BuildHasher, Hasher},
          path::{Path, PathBuf},
          sync::{Arc, Mutex,
                 atomic::{AtomicI64, AtomicU64, Ordering}},
          time::{SystemTime, UNIX_EPOCH}};

use arc_swap::ArcSwap;
use rusqlite::{Connection, params};
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub keyword: String,
    pub url: String,
    pub owner: String,
    pub description: String,
    pub private: bool,
    /// Named by the app (paste a URL without a name), not by a person.
    pub generated: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Link {
    pub fn new(keyword: &str, url: &str, owner: &str) -> Self {
        Self { keyword: keyword.into(),
               url: url.into(),
               owner: owner.into(),
               description: String::new(),
               private: false,
               generated: false,
               created_at: 0,
               updated_at: 0,
               updated_by: String::new() }
    }
}

#[derive(Debug, Default)]
struct Hits {
    count: AtomicU64,
    flushed: AtomicU64,
    /// Unix seconds of the last redirect (0 = never), from the coarse clock.
    last_used: AtomicI64,
}

pub struct Entry {
    pub link: Link,
    hits: Arc<Hits>,
    /// Other names for this link, sorted. Each is a key of the map pointing at this same entry.
    pub aliases: Vec<String>,
    /// The newest HISTORY_KEEP changes, newest first (also in the `history` table, which keeps all of them): the edit
    /// page reads this, so it never waits on the database either.
    pub history: Vec<Change>,
}

pub const HISTORY_KEEP: usize = 10;

/// A clock for the hot path: unix seconds, refreshed every second by `tick()` (main's background task). Reading it is
/// one relaxed load instead of a clock call per redirect; "last used" doesn't need better than a second.
static CLOCK: AtomicI64 = AtomicI64::new(0);

pub fn tick() {
    CLOCK.store(now(), Ordering::Relaxed);
}

#[inline]
fn coarse_now() -> i64 {
    match CLOCK.load(Ordering::Relaxed) {
        0 => now(),
        t => t,
    }
}

impl Entry {
    #[inline]
    pub fn hit(&self) {
        self.hits.count.fetch_add(1, Ordering::Relaxed);
        self.hits.last_used.store(coarse_now(), Ordering::Relaxed);
    }

    pub fn hits(&self) -> u64 {
        self.hits.count.load(Ordering::Relaxed)
    }

    /// Unix seconds of the last redirect, 0 = never.
    pub fn last_used(&self) -> i64 {
        self.hits.last_used.load(Ordering::Relaxed)
    }
}

/// Keys are shared strings, so the copy an edit makes of the map copies pointers, not names.
type Map = HashMap<Arc<str>, Arc<Entry>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub url: String,
    pub owner: String,
}

#[derive(Debug)]
pub enum StoreError {
    Db(rusqlite::Error),
    Io(std::io::Error),
    Task,
    /// The name is already used by another link (as its name or one of its aliases).
    Taken(String),
    NotFound,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::Io(e) => write!(f, "file: {e}"),
            Self::Task => write!(f, "database task failed"),
            Self::Taken(by) => write!(f, "that name is already used by go/{by}"),
            Self::NotFound => write!(f, "no such link"),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS links (
    keyword     TEXT PRIMARY KEY NOT NULL,
    url         TEXT NOT NULL,
    owner       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    private     INTEGER NOT NULL DEFAULT 0,
    hits        INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS history (
    id          INTEGER PRIMARY KEY,
    keyword     TEXT NOT NULL,
    action      TEXT NOT NULL,
    url         TEXT NOT NULL,
    owner       TEXT NOT NULL,
    description TEXT NOT NULL,
    private     INTEGER NOT NULL,
    changed_at  INTEGER NOT NULL,
    actor       TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS history_keyword ON history (keyword, id);
";

/// Schema changes after the first release, in order; PRAGMA user_version = how many have run.
const MIGRATIONS: &[&str] = &[
    // 1 (2026-10-11): last used, app-generated names, aliases
    "ALTER TABLE links ADD COLUMN last_used INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE links ADD COLUMN generated INTEGER NOT NULL DEFAULT 0;
     CREATE TABLE aliases (alias TEXT PRIMARY KEY NOT NULL, keyword TEXT NOT NULL) STRICT;
     CREATE INDEX aliases_keyword ON aliases (keyword);",
];

fn migrate(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let done: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(done.max(0) as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        info!(version = i + 1, "database migrated");
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The newest HISTORY_KEEP changes of every keyword, newest first (once, at start).
fn load_history(conn: &Connection) -> Result<HashMap<String, Vec<Change>>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached("SELECT keyword, changed_at, actor, action, url, owner FROM (SELECT *, row_number() \
                                        OVER (PARTITION BY keyword ORDER BY id DESC) AS n FROM history) \
                                        WHERE n <= ?1 ORDER BY keyword, id DESC")?;
    let rows = stmt.query_map(params![HISTORY_KEEP as i64], |r| {
                       Ok((r.get::<_, String>(0)?,
                           Change { at: r.get(1)?, actor: r.get(2)?, action: r.get(3)?, url: r.get(4)?, owner: r.get(5)? }))
                   })?;
    let mut out: HashMap<String, Vec<Change>> = HashMap::new();
    for row in rows {
        let (k, c) = row?;
        out.entry(k).or_default().push(c);
    }
    Ok(out)
}

/// The newest HISTORY_KEEP changes of one keyword (index history_keyword: cost doesn't grow with the table).
fn history_of(conn: &Connection, keyword: &str) -> Result<Vec<Change>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached("SELECT changed_at, actor, action, url, owner FROM history WHERE keyword = ?1 \
                                        ORDER BY id DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![keyword, HISTORY_KEEP as i64], |r| {
                       Ok(Change { at: r.get(0)?, actor: r.get(1)?, action: r.get(2)?, url: r.get(3)?, owner: r.get(4)? })
                   })?;
    rows.collect()
}

fn log_change(tx: &rusqlite::Transaction, link: &Link, action: &str, at: i64, actor: &str) -> Result<(), rusqlite::Error> {
    tx.execute("INSERT INTO history (keyword, action, url, owner, description, private, changed_at, actor) \
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
               params![link.keyword, action, link.url, link.owner, link.description, link.private as i64, at, actor])?;
    Ok(())
}

/// Random lowercase names for links saved without one: 5 characters from 31 unambiguous ones (no 0/o, 1/l/i).
/// The randomness is std's per-process SipHash keys (seeded from the OS), so no RNG crate is needed.
fn random_name(seed: u64) -> String {
    const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u64(seed);
    h.write_i64(now());
    let mut x = h.finish();
    let mut s = String::with_capacity(5);
    for _ in 0..5 {
        s.push(ALPHABET[(x % ALPHABET.len() as u64) as usize] as char);
        x /= ALPHABET.len() as u64;
    }
    s
}

pub struct Store {
    map: ArcSwap<Map>,
    db: Arc<Mutex<Connection>>,
    /// Serialises edits (database write + map swap), so two edits can't swap in each other's stale map.
    writer: tokio::sync::Mutex<()>,
    /// Where `snapshot` writes a consistent copy for the backups (`<db dir>/snapshot/links.db`).
    snapshot: PathBuf,
    /// Bumped by every edit; the snapshot task compares it with the last one it saved.
    pub generation: AtomicU64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut conn = Connection::open(path)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            warn!(mode, "SQLite did not switch to WAL");
        }
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA busy_timeout = 5000;")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&mut conn)?;

        let mut map = Map::new();
        let mut links = 0usize;
        {
            let mut history = load_history(&conn)?;
            let mut aliases: HashMap<String, Vec<String>> = HashMap::new();
            let mut stmt = conn.prepare("SELECT alias, keyword FROM aliases ORDER BY alias")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (a, k) = row?;
                aliases.entry(k).or_default().push(a);
            }
            let mut stmt = conn.prepare("SELECT keyword, url, owner, description, private, hits, created_at, updated_at, \
                                         updated_by, last_used, generated FROM links")?;
            let rows = stmt.query_map([], |r| {
                               let hits: i64 = r.get(5)?;
                               Ok((Link { keyword: r.get(0)?,
                                          url: r.get(1)?,
                                          owner: r.get(2)?,
                                          description: r.get(3)?,
                                          private: r.get::<_, i64>(4)? != 0,
                                          generated: r.get::<_, i64>(10)? != 0,
                                          created_at: r.get(6)?,
                                          updated_at: r.get(7)?,
                                          updated_by: r.get(8)? },
                                   hits.max(0) as u64,
                                   r.get::<_, i64>(9)?))
                           })?;
            for row in rows {
                let (link, hits, last_used) = row?;
                let counter = Arc::new(Hits { count: AtomicU64::new(hits),
                                              flushed: AtomicU64::new(hits),
                                              last_used: AtomicI64::new(last_used) });
                let entry = Arc::new(Entry { history: history.remove(&link.keyword).unwrap_or_default(),
                                             aliases: aliases.remove(&link.keyword).unwrap_or_default(),
                                             link,
                                             hits: counter });
                for a in &entry.aliases {
                    map.insert(Arc::from(a.as_str()), entry.clone());
                }
                map.insert(Arc::from(entry.link.keyword.as_str()), entry);
                links += 1;
            }
            for (k, orphans) in aliases {
                warn!(keyword = k, ?orphans, "aliases of a missing link ignored");
            }
        }
        info!(links, names = map.len(), db = %path.display(), "links loaded");

        let snapshot = path.parent().unwrap_or(Path::new(".")).join("snapshot").join("links.db");
        Ok(Self { map: ArcSwap::from_pointee(map),
                  db: Arc::new(Mutex::new(conn)),
                  writer: tokio::sync::Mutex::new(()),
                  snapshot,
                  generation: AtomicU64::new(0) })
    }

    /// The hot path: no lock, no allocation. A name or an alias; the entry's `link.keyword` is the link's own name.
    #[inline]
    pub fn get(&self, name: &str) -> Option<Arc<Entry>> {
        self.map.load().get(name).cloned()
    }

    /// Number of links (not counting aliases).
    pub fn len(&self) -> usize {
        self.map.load().iter().filter(|(k, e)| ***k == *e.link.keyword).count()
    }

    /// Every link once (aliases folded into their link), most used first (ties: alphabetical).
    pub fn all(&self) -> Vec<Arc<Entry>> {
        let mut out: Vec<Arc<Entry>> =
            self.map.load().iter().filter(|(k, e)| ***k == *e.link.keyword).map(|(_, e)| e.clone()).collect();
        out.sort_by(|a, b| b.hits().cmp(&a.hits()).then_with(|| a.link.keyword.cmp(&b.link.keyword)));
        out
    }

    async fn with_db<T, F>(&self, f: F) -> Result<T, StoreError>
        where T: Send + 'static,
              F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().unwrap_or_else(|p| p.into_inner());
            f(&mut conn)
        }).await
          .map_err(|_| StoreError::Task)?
    }

    /// Put `entry` in the map under its name and all its aliases, dropping `gone` names. Caller holds `writer`.
    fn publish(&self, entry: Option<Arc<Entry>>, gone: &[String]) {
        let mut next = Map::clone(&self.map.load());
        for g in gone {
            next.remove(g.as_str());
        }
        if let Some(e) = entry {
            for a in &e.aliases {
                next.insert(Arc::from(a.as_str()), e.clone());
            }
            next.insert(Arc::from(e.link.keyword.as_str()), e);
        }
        self.map.store(Arc::new(next));
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// A free name for a link saved without one.
    pub fn free_name(&self) -> String {
        let map = self.map.load();
        (0u64..).map(random_name).find(|n| !map.contains_key(n.as_str())).unwrap_or_default()
    }

    /// Create or replace a link (the caller has checked who may). Keeps created_at, the counters and the aliases.
    pub async fn save(&self, mut link: Link, actor: &str) -> Result<Arc<Entry>, StoreError> {
        let _guard = self.writer.lock().await;
        let old = self.get(&link.keyword);
        if let Some(o) = &old {
            if o.link.keyword != link.keyword {
                return Err(StoreError::Taken(o.link.keyword.clone())); // it's another link's alias
            }
        }
        let t = now();
        link.created_at = old.as_ref().map_or(t, |e| e.link.created_at);
        link.generated = old.as_ref().map_or(link.generated, |e| e.link.generated);
        link.updated_at = t;
        link.updated_by = actor.to_string();
        let action = if old.is_some() { "update" } else { "create" };

        let row = link.clone();
        let fresh = old.is_none();
        let loaded = self.with_db(move |conn| {
                             let tx = conn.transaction()?;
                             tx.execute("INSERT INTO links (keyword, url, owner, description, private, hits, created_at, \
                                         updated_at, updated_by, generated) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8, ?9)
                                         ON CONFLICT (keyword) DO UPDATE SET url = ?2, owner = ?3, description = ?4,
                                         private = ?5, updated_at = ?7, updated_by = ?8",
                                        params![row.keyword,
                                                row.url,
                                                row.owner,
                                                row.description,
                                                row.private as i64,
                                                row.created_at,
                                                row.updated_at,
                                                row.updated_by,
                                                row.generated as i64])?;
                             log_change(&tx, &row, action, row.updated_at, &row.updated_by)?;
                             // a new (or re-created) keyword: its earlier life (deletes included) comes from the table, once
                             let loaded = if fresh { Some(history_of(&tx, &row.keyword)?) } else { None };
                             tx.commit()?;
                             Ok(loaded)
                         })
                         .await?;

        let change = Change { at: link.updated_at,
                              actor: link.updated_by.clone(),
                              action: action.to_string(),
                              url: link.url.clone(),
                              owner: link.owner.clone() };
        let history = match (&old, loaded) {
            (_, Some(h)) => h,
            (Some(e), None) => std::iter::once(change).chain(e.history.iter().cloned()).take(HISTORY_KEEP).collect(),
            (None, None) => vec![change],
        };
        let (hits, aliases) = old.map_or_else(|| (Arc::new(Hits::default()), Vec::new()), |e| (e.hits.clone(), e.aliases.clone()));
        let entry = Arc::new(Entry { link, hits, aliases, history });
        self.publish(Some(entry.clone()), &[]);
        Ok(entry)
    }

    /// Delete a link and its aliases. `keyword` must be the link's own name.
    pub async fn delete(&self, keyword: &str, actor: &str) -> Result<bool, StoreError> {
        let _guard = self.writer.lock().await;
        let Some(old) = self.get(keyword).filter(|e| e.link.keyword == keyword) else { return Ok(false) };
        let (link, actor, t) = (old.link.clone(), actor.to_string(), now());
        self.with_db(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("DELETE FROM links WHERE keyword = ?1", params![link.keyword])?;
                tx.execute("DELETE FROM aliases WHERE keyword = ?1", params![link.keyword])?;
                log_change(&tx, &link, "delete", t, &actor)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let mut gone = old.aliases.clone();
        gone.push(old.link.keyword.clone());
        self.publish(None, &gone);
        Ok(true)
    }

    /// Give a link (its own name `keyword`) another name. The caller has checked who may and that `alias` is valid.
    pub async fn add_alias(&self, keyword: &str, alias: &str, actor: &str) -> Result<Arc<Entry>, StoreError> {
        let _guard = self.writer.lock().await;
        let old = self.get(keyword).filter(|e| e.link.keyword == keyword).ok_or(StoreError::NotFound)?;
        if let Some(other) = self.get(alias) {
            return Err(StoreError::Taken(other.link.keyword.clone()));
        }
        let (link, a, who, t) = (old.link.clone(), alias.to_string(), actor.to_string(), now());
        let action = format!("alias +{alias}");
        let act = action.clone();
        self.with_db(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("INSERT INTO aliases (alias, keyword) VALUES (?1, ?2)", params![a, link.keyword])?;
                log_change(&tx, &link, &act, t, &who)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let mut aliases = old.aliases.clone();
        aliases.push(alias.to_string());
        aliases.sort();
        let change = Change { at: t, actor: actor.into(), action, url: old.link.url.clone(), owner: old.link.owner.clone() };
        let history = std::iter::once(change).chain(old.history.iter().cloned()).take(HISTORY_KEEP).collect();
        let entry = Arc::new(Entry { link: old.link.clone(), hits: old.hits.clone(), aliases, history });
        self.publish(Some(entry.clone()), &[]);
        Ok(entry)
    }

    pub async fn remove_alias(&self, alias: &str, actor: &str) -> Result<Arc<Entry>, StoreError> {
        let _guard = self.writer.lock().await;
        let old = self.get(alias).filter(|e| e.link.keyword != alias).ok_or(StoreError::NotFound)?;
        let (link, a, who, t) = (old.link.clone(), alias.to_string(), actor.to_string(), now());
        let action = format!("alias -{alias}");
        let act = action.clone();
        let gone = self.with_db(move |conn| {
                           let tx = conn.transaction()?;
                           let n = tx.execute("DELETE FROM aliases WHERE alias = ?1", params![a])?;
                           log_change(&tx, &link, &act, t, &who)?;
                           tx.commit()?;
                           Ok(n)
                       })
                       .await?;
        if gone == 0 {
            warn!(alias, "alias was in memory but not in the database");
        }
        let aliases = old.aliases.iter().filter(|x| *x != alias).cloned().collect();
        let change = Change { at: t, actor: actor.into(), action, url: old.link.url.clone(), owner: old.link.owner.clone() };
        let history = std::iter::once(change).chain(old.history.iter().cloned()).take(HISTORY_KEEP).collect();
        let entry = Arc::new(Entry { link: old.link.clone(), hits: old.hits.clone(), aliases, history });
        self.publish(Some(entry.clone()), &[alias.to_string()]);
        Ok(entry)
    }

    /// Write the hit counts (and last-used times) that changed since the last flush. Returns how many rows.
    pub async fn flush_hits(&self) -> Result<usize, StoreError> {
        let dirty: Vec<(String, u64, i64, Arc<Hits>)> =
            self.map
                .load()
                .iter()
                .filter(|(k, e)| ***k == *e.link.keyword)
                .filter_map(|(_, e)| {
                    let n = e.hits.count.load(Ordering::Relaxed);
                    (n != e.hits.flushed.load(Ordering::Relaxed)).then(|| {
                                                                     (e.link.keyword.clone(),
                                                                      n,
                                                                      e.hits.last_used.load(Ordering::Relaxed),
                                                                      e.hits.clone())
                                                                 })
                })
                .collect();
        if dirty.is_empty() {
            return Ok(0);
        }
        let n = dirty.len();
        self.with_db(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare_cached("UPDATE links SET hits = ?2, last_used = ?3 WHERE keyword = ?1")?;
                    for (keyword, hits, last, _) in &dirty {
                        stmt.execute(params![keyword, *hits as i64, last])?;
                    }
                }
                tx.commit()?;
                for (_, hits, _, counter) in &dirty {
                    counter.flushed.store(*hits, Ordering::Relaxed);
                }
                Ok(())
            })
            .await?;
        Ok(n)
    }

    /// A consistent copy of the database for the backups (`VACUUM INTO` a temp file, then rename over the old one).
    pub async fn snapshot(&self) -> Result<(), StoreError> {
        let dest = self.snapshot.clone();
        self.with_db(move |conn| {
                let dir = dest.parent().unwrap_or(Path::new("."));
                std::fs::create_dir_all(dir)?;
                let tmp = dir.join(".links.db.tmp");
                let _ = std::fs::remove_file(&tmp);
                conn.execute("VACUUM INTO ?1", params![tmp.to_string_lossy()])?;
                std::fs::rename(&tmp, &dest)?;
                Ok(())
            })
            .await
    }

    /// The schema version (for tests and the startup log).
    #[cfg(test)]
    async fn user_version(&self) -> i64 {
        self.with_db(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))
            .await
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("linkshrink-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn save_hit_flush_reload() {
        let dir = tmpdir("reload");
        let db = dir.join("links.db");
        {
            let s = Store::open(&db).unwrap();
            assert_eq!(s.user_version().await, MIGRATIONS.len() as i64);
            s.save(Link::new("wiki", "https://wiki.example/", "alice"), "alice").await.unwrap();
            for _ in 0..3 {
                s.get("wiki").unwrap().hit();
            }
            // an edit keeps the counter, and hits during/after it still count
            s.save(Link::new("wiki", "https://wiki.example/new", "alice"), "alice").await.unwrap();
            s.get("wiki").unwrap().hit();
            assert_eq!(s.get("wiki").unwrap().hits(), 4);
            assert!(s.get("wiki").unwrap().last_used() > 0);
            assert_eq!(s.flush_hits().await.unwrap(), 1);
            assert_eq!(s.flush_hits().await.unwrap(), 0, "nothing new to write");
            s.snapshot().await.unwrap();
            assert!(dir.join("snapshot/links.db").exists());
        }
        let s = Store::open(&db).unwrap();
        let e = s.get("wiki").unwrap();
        assert_eq!(e.link.url, "https://wiki.example/new");
        assert_eq!(e.hits(), 4);
        assert!(e.last_used() > 0, "last used survives a restart");
        assert!(e.link.created_at > 0 && e.link.updated_by == "alice");
        let h = &s.get("wiki").unwrap().history;
        assert_eq!(h.iter().map(|c| c.action.as_str()).collect::<Vec<_>>(), ["update", "create"]);

        assert!(s.delete("wiki", "bob").await.unwrap());
        assert!(s.get("wiki").is_none());
        assert!(!s.delete("wiki", "bob").await.unwrap());
        // re-created: its history includes the delete (read back from the table)
        s.save(Link::new("wiki", "https://wiki.example/again", "carol"), "carol").await.unwrap();
        let h = &s.get("wiki").unwrap().history;
        assert_eq!(h.iter().map(|c| (c.action.as_str(), c.actor.as_str())).collect::<Vec<_>>(),
                   [("create", "carol"), ("delete", "bob"), ("update", "alice"), ("create", "alice")]);
        for i in 0..12 {
            s.save(Link::new("wiki", &format!("https://wiki.example/{i}"), "carol"), "carol").await.unwrap();
        }
        assert_eq!(s.get("wiki").unwrap().history.len(), HISTORY_KEEP);
        assert_eq!(s.get("wiki").unwrap().history[0].url, "https://wiki.example/11");
        // the snapshot is a complete database of its own
        let snap = Store::open(&dir.join("snapshot/links.db")).unwrap();
        assert_eq!(snap.get("wiki").unwrap().hits(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn aliases_share_the_link() {
        let dir = tmpdir("aliases");
        let db = dir.join("links.db");
        {
            let s = Store::open(&db).unwrap();
            s.save(Link::new("wiki", "https://wiki.example/", "alice"), "alice").await.unwrap();
            s.add_alias("wiki", "docs", "alice").await.unwrap();
            s.add_alias("wiki", "kb", "alice").await.unwrap();
            let (a, b) = (s.get("docs").unwrap(), s.get("wiki").unwrap());
            assert!(Arc::ptr_eq(&a, &b), "an alias is the same entry");
            a.hit();
            assert_eq!(s.get("kb").unwrap().hits(), 1);
            assert_eq!(s.len(), 1);
            assert_eq!(s.all().len(), 1);
            // names are one namespace
            assert!(matches!(s.add_alias("wiki", "kb", "alice").await, Err(StoreError::Taken(_))));
            assert!(matches!(s.save(Link::new("docs", "https://x.example/", "bob"), "bob").await, Err(StoreError::Taken(k)) if k == "wiki"));
            // an edit keeps the aliases
            s.save(Link::new("wiki", "https://wiki.example/2", "alice"), "alice").await.unwrap();
            assert_eq!(s.get("docs").unwrap().link.url, "https://wiki.example/2");
            s.remove_alias("kb", "alice").await.unwrap();
            assert!(s.get("kb").is_none());
            assert!(matches!(s.remove_alias("wiki", "alice").await, Err(StoreError::NotFound)), "a name isn't an alias");
            s.flush_hits().await.unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.get("wiki").unwrap().aliases, ["docs"]);
        assert!(Arc::ptr_eq(&s.get("docs").unwrap(), &s.get("wiki").unwrap()));
        assert_eq!(s.get("docs").unwrap().hits(), 1);
        assert!(s.get("wiki").unwrap().history.iter().any(|c| c.action == "alias -kb"));
        // deleting the link deletes its aliases
        s.delete("wiki", "alice").await.unwrap();
        assert!(s.get("docs").is_none());
        drop(s);
        assert!(Store::open(&db).unwrap().get("docs").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn migrates_a_first_release_database() {
        let dir = tmpdir("migrate");
        let db = dir.join("links.db");
        {
            // the schema as 0.2.0 shipped it, with one link
            let c = Connection::open(&db).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            c.execute("INSERT INTO links (keyword, url, owner, hits, created_at, updated_at, updated_by) \
                       VALUES ('wiki', 'https://wiki.example/', 'alice', 7, 1, 1, 'alice')",
                      [])
             .unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.user_version().await, 1);
        let e = s.get("wiki").unwrap();
        assert_eq!((e.hits(), e.last_used(), e.link.generated), (7, 0, false));
        s.add_alias("wiki", "docs", "alice").await.unwrap();
        drop(s);
        assert!(Store::open(&db).is_ok(), "re-opening doesn't migrate twice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn random_names_look_right() {
        for i in 0..100 {
            let n = random_name(i);
            assert_eq!(n.len(), 5);
            assert!(n.chars().all(|c| "23456789abcdefghjkmnpqrstuvwxyz".contains(c)), "{n}");
        }
        assert_ne!(random_name(1), random_name(2));
    }

    #[tokio::test]
    async fn concurrent_hits_are_not_lost() {
        let dir = tmpdir("concurrent");
        let s = Arc::new(Store::open(&dir.join("links.db")).unwrap());
        s.save(Link::new("a", "https://a.example/", "alice"), "alice").await.unwrap();
        s.add_alias("a", "b", "alice").await.unwrap();
        let mut tasks = Vec::new();
        for i in 0..8 {
            let s = s.clone();
            tasks.push(tokio::spawn(async move {
                          for _ in 0..1000 {
                              s.get(if i % 2 == 0 { "a" } else { "b" }).unwrap().hit();
                          }
                      }));
        }
        // an edit in the middle of the storm
        s.save(Link::new("a", "https://a.example/2", "alice"), "alice").await.unwrap();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(s.get("a").unwrap().hits(), 8000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
