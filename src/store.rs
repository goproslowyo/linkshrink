//! The links. Every link lives in memory, in an immutable map behind an atomic pointer swap: a redirect is one
//! atomic load, one hash lookup and one atomic add, with no lock and no I/O. SQLite is the source of truth on
//! disk: edits are written through (database first, then a new map is swapped in), and hit counts are added up in
//! memory and written back every few seconds, only for the links that were used (d0nut's original design kept the
//! hits in its cache and flushed them to Redis the same way).
//!
//! A link's hit counter is shared between the old and the new map entry, so an edit never loses or reverts hits,
//! and the hit flush only ever writes the `hits` column, so it can't revert an edit either.

use std::{collections::HashMap,
          fmt,
          path::{Path, PathBuf},
          sync::{Arc, Mutex,
                 atomic::{AtomicU64, Ordering}},
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
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

#[derive(Debug, Default)]
struct Hits {
    count: AtomicU64,
    flushed: AtomicU64,
}

pub struct Entry {
    pub link: Link,
    hits: Arc<Hits>,
    /// The newest HISTORY_KEEP changes, newest first (also in the `history` table, which keeps all of them): the edit
    /// page reads this, so it never waits on the database either.
    pub history: Vec<Change>,
}

pub const HISTORY_KEEP: usize = 10;

impl Entry {
    #[inline]
    pub fn hit(&self) {
        self.hits.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn hits(&self) -> u64 {
        self.hits.count.load(Ordering::Relaxed)
    }
}

type Map = HashMap<String, Arc<Entry>>;

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
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::Io(e) => write!(f, "file: {e}"),
            Self::Task => write!(f, "database task failed"),
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

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// The newest HISTORY_KEEP changes per keyword (one keyword, or all), newest first.
fn load_history(conn: &Connection, keyword: Option<&str>) -> Result<HashMap<String, Vec<Change>>, rusqlite::Error> {
    let mut stmt = conn.prepare_cached("SELECT keyword, changed_at, actor, action, url, owner FROM (SELECT *, row_number() \
                                        OVER (PARTITION BY keyword ORDER BY id DESC) AS n FROM history \
                                        WHERE ?1 IS NULL OR keyword = ?1) WHERE n <= ?2 ORDER BY keyword, id DESC")?;
    let rows = stmt.query_map(params![keyword, HISTORY_KEEP as i64], |r| {
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
        let conn = Connection::open(path)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            warn!(mode, "SQLite did not switch to WAL");
        }
        conn.execute_batch("PRAGMA synchronous = FULL; PRAGMA busy_timeout = 5000;")?;
        conn.execute_batch(SCHEMA)?;

        let mut map = Map::new();
        {
            let mut stmt = conn.prepare("SELECT keyword, url, owner, description, private, hits, created_at, \
                                         updated_at, updated_by FROM links")?;
            let rows = stmt.query_map([], |r| {
                               let hits: i64 = r.get(5)?;
                               Ok((Link { keyword: r.get(0)?,
                                          url: r.get(1)?,
                                          owner: r.get(2)?,
                                          description: r.get(3)?,
                                          private: r.get::<_, i64>(4)? != 0,
                                          created_at: r.get(6)?,
                                          updated_at: r.get(7)?,
                                          updated_by: r.get(8)? },
                                   hits.max(0) as u64))
                           })?;
            let mut history = load_history(&conn, None)?;
            for row in rows {
                let (link, hits) = row?;
                let counter = Arc::new(Hits { count: AtomicU64::new(hits), flushed: AtomicU64::new(hits) });
                let history = history.remove(&link.keyword).unwrap_or_default();
                map.insert(link.keyword.clone(), Arc::new(Entry { link, hits: counter, history }));
            }
        }
        info!(links = map.len(), db = %path.display(), "links loaded");

        let snapshot = path.parent().unwrap_or(Path::new(".")).join("snapshot").join("links.db");
        Ok(Self { map: ArcSwap::from_pointee(map),
                  db: Arc::new(Mutex::new(conn)),
                  writer: tokio::sync::Mutex::new(()),
                  snapshot,
                  generation: AtomicU64::new(0) })
    }

    /// The hot path: no lock, no allocation.
    #[inline]
    pub fn get(&self, keyword: &str) -> Option<Arc<Entry>> {
        self.map.load().get(keyword).cloned()
    }

    pub fn len(&self) -> usize {
        self.map.load().len()
    }

    /// Every link, most used first (ties: alphabetical).
    pub fn all(&self) -> Vec<Arc<Entry>> {
        let mut out: Vec<Arc<Entry>> = self.map.load().values().cloned().collect();
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

    /// Create or replace a link (the caller has checked who may). Keeps the created_at and the hit count.
    pub async fn save(&self, mut link: Link, actor: &str) -> Result<Arc<Entry>, StoreError> {
        let _guard = self.writer.lock().await;
        let old = self.get(&link.keyword);
        let t = now();
        link.created_at = old.as_ref().map_or(t, |e| e.link.created_at);
        link.updated_at = t;
        link.updated_by = actor.to_string();
        let action = if old.is_some() { "update" } else { "create" };

        let row = link.clone();
        let fresh = old.is_none();
        let loaded = self.with_db(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("INSERT INTO links (keyword, url, owner, description, private, hits, created_at, updated_at, \
                            updated_by) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)
                            ON CONFLICT (keyword) DO UPDATE SET url = ?2, owner = ?3, description = ?4, private = ?5,
                            updated_at = ?7, updated_by = ?8",
                           params![row.keyword,
                                   row.url,
                                   row.owner,
                                   row.description,
                                   row.private as i64,
                                   row.created_at,
                                   row.updated_at,
                                   row.updated_by])?;
                tx.execute("INSERT INTO history (keyword, action, url, owner, description, private, changed_at, actor) \
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                           params![row.keyword,
                                   action,
                                   row.url,
                                   row.owner,
                                   row.description,
                                   row.private as i64,
                                   row.updated_at,
                                   row.updated_by])?;
                // a new (or re-created) keyword: its earlier life (deletes included) comes from the table, once
                let loaded = if fresh { load_history(&tx, Some(&row.keyword))?.remove(&row.keyword) } else { None };
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
        let hits = old.map_or_else(|| Arc::new(Hits::default()), |e| e.hits.clone());
        let entry = Arc::new(Entry { link, hits, history });
        let mut next = Map::clone(&self.map.load());
        next.insert(entry.link.keyword.clone(), entry.clone());
        self.map.store(Arc::new(next));
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(entry)
    }

    pub async fn delete(&self, keyword: &str, actor: &str) -> Result<bool, StoreError> {
        let _guard = self.writer.lock().await;
        let Some(old) = self.get(keyword) else { return Ok(false) };
        let (link, actor, t) = (old.link.clone(), actor.to_string(), now());
        self.with_db(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("DELETE FROM links WHERE keyword = ?1", params![link.keyword])?;
                tx.execute("INSERT INTO history (keyword, action, url, owner, description, private, changed_at, actor) \
                            VALUES (?1, 'delete', ?2, ?3, ?4, ?5, ?6, ?7)",
                           params![link.keyword, link.url, link.owner, link.description, link.private as i64, t, actor])?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let mut next = Map::clone(&self.map.load());
        next.remove(keyword);
        self.map.store(Arc::new(next));
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    }

    /// Write the hit counts that changed since the last flush. Returns how many rows were written.
    pub async fn flush_hits(&self) -> Result<usize, StoreError> {
        let dirty: Vec<(String, u64, Arc<Hits>)> =
            self.map
                .load()
                .values()
                .filter_map(|e| {
                    let n = e.hits.count.load(Ordering::Relaxed);
                    (n != e.hits.flushed.load(Ordering::Relaxed)).then(|| (e.link.keyword.clone(), n, e.hits.clone()))
                })
                .collect();
        if dirty.is_empty() {
            return Ok(0);
        }
        let n = dirty.len();
        self.with_db(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare_cached("UPDATE links SET hits = ?2 WHERE keyword = ?1")?;
                    for (keyword, hits, _) in &dirty {
                        stmt.execute(params![keyword, *hits as i64])?;
                    }
                }
                tx.commit()?;
                for (_, hits, counter) in &dirty {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(keyword: &str, url: &str, owner: &str) -> Link {
        Link { keyword: keyword.into(),
               url: url.into(),
               owner: owner.into(),
               description: String::new(),
               private: false,
               created_at: 0,
               updated_at: 0,
               updated_by: String::new() }
    }

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
            s.save(link("wiki", "https://wiki.example/", "alice"), "alice").await.unwrap();
            for _ in 0..3 {
                s.get("wiki").unwrap().hit();
            }
            // an edit keeps the counter, and hits during/after it still count
            s.save(link("wiki", "https://wiki.example/new", "alice"), "alice").await.unwrap();
            s.get("wiki").unwrap().hit();
            assert_eq!(s.get("wiki").unwrap().hits(), 4);
            assert_eq!(s.flush_hits().await.unwrap(), 1);
            assert_eq!(s.flush_hits().await.unwrap(), 0, "nothing new to write");
            s.snapshot().await.unwrap();
            assert!(dir.join("snapshot/links.db").exists());
        }
        let s = Store::open(&db).unwrap();
        let e = s.get("wiki").unwrap();
        assert_eq!(e.link.url, "https://wiki.example/new");
        assert_eq!(e.hits(), 4);
        assert!(e.link.created_at > 0 && e.link.updated_by == "alice");
        let h = &s.get("wiki").unwrap().history;
        assert_eq!(h.iter().map(|c| c.action.as_str()).collect::<Vec<_>>(), ["update", "create"]);

        assert!(s.delete("wiki", "bob").await.unwrap());
        assert!(s.get("wiki").is_none());
        assert!(!s.delete("wiki", "bob").await.unwrap());
        // re-created: its history includes the delete (read back from the table)
        s.save(link("wiki", "https://wiki.example/again", "carol"), "carol").await.unwrap();
        let h = &s.get("wiki").unwrap().history;
        assert_eq!(h.iter().map(|c| (c.action.as_str(), c.actor.as_str())).collect::<Vec<_>>(),
                   [("create", "carol"), ("delete", "bob"), ("update", "alice"), ("create", "alice")]);
        for i in 0..12 {
            s.save(link("wiki", &format!("https://wiki.example/{i}"), "carol"), "carol").await.unwrap();
        }
        assert_eq!(s.get("wiki").unwrap().history.len(), HISTORY_KEEP);
        assert_eq!(s.get("wiki").unwrap().history[0].url, "https://wiki.example/11");
        // the snapshot is a complete database of its own
        let snap = Store::open(&dir.join("snapshot/links.db")).unwrap();
        assert_eq!(snap.get("wiki").unwrap().hits(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_hits_are_not_lost() {
        let dir = tmpdir("concurrent");
        let s = Arc::new(Store::open(&dir.join("links.db")).unwrap());
        s.save(link("a", "https://a.example/", "alice"), "alice").await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            tasks.push(tokio::spawn(async move {
                          for _ in 0..1000 {
                              s.get("a").unwrap().hit();
                          }
                      }));
        }
        // an edit in the middle of the storm
        s.save(link("a", "https://a.example/2", "alice"), "alice").await.unwrap();
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(s.get("a").unwrap().hits(), 8000);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
