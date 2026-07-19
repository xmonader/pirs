//! Persistent, incrementally-refreshed backing store for the code graph.
//!
//! The store is a **disposable cache** of parsed symbols keyed by file. It never
//! owns truth — the source files do — so a stale or corrupt store can only ever
//! degrade retrieval, never corrupt the repo. On any schema mismatch or open
//! error it is rebuilt from scratch.
//!
//! Incrementality is per file: on every refresh the store stat-walks the tree
//! (gitignore-aware), re-parses only files whose `(size, mtime)` changed since
//! last time, drops symbols for deleted files, and leaves every unchanged file's
//! symbols untouched. Parsing is the cost that scales with repo size; the store
//! exists to skip it for the ~all files that didn't change between turns.
//!
//! The loaded symbol set is fed through [`Graph::from_symbols`], the *same*
//! constructor a full parse uses, so an incrementally-refreshed graph and a
//! from-scratch one over the same tree are structurally identical.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::graph::{parse_file, parse_tree};
use crate::{Graph, SymKind, Symbol};

/// Bump when the on-disk layout or symbol encoding changes; a mismatch nukes and
/// rebuilds the cache rather than risking a misread.
const SCHEMA_VERSION: &str = "1";

pub struct GraphStore {
    conn: Connection,
    root: PathBuf,
}

/// The change set a refresh computed, for logging/tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RefreshStats {
    pub reparsed: usize,
    pub deleted: usize,
    pub unchanged: usize,
}

impl GraphStore {
    /// Open (or create) the store at `db_path`. On any incompatibility the file
    /// is wiped and recreated — the cache is always safe to discard.
    pub fn open(db_path: &Path, root: &Path) -> Result<GraphStore> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = match Self::open_verified(db_path) {
            Ok(conn) => conn,
            Err(_) => {
                // Corrupt or wrong-version: start clean.
                std::fs::remove_file(db_path).ok();
                Self::open_verified(db_path)
                    .with_context(|| format!("recreating graph store at {}", db_path.display()))?
            }
        };
        Ok(GraphStore {
            conn,
            root: root.to_path_buf(),
        })
    }

    fn open_verified(db_path: &Path) -> Result<Connection> {
        let conn = Connection::open(db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS files (
                 path TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS symbols (
                 file TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL,
                 line INTEGER NOT NULL, start_byte INTEGER NOT NULL,
                 end_byte INTEGER NOT NULL, calls TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);",
        )?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        match version {
            Some(v) if v == SCHEMA_VERSION => {}
            Some(_) => anyhow::bail!("schema version mismatch"),
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    [SCHEMA_VERSION],
                )?;
            }
        }
        Ok(conn)
    }

    /// Incrementally reconcile the store with the tree and return the loaded
    /// symbol set. Always stat-walks (stat is cheap; skipping the walk is how you
    /// silently miss an external edit), re-parses only changed/new files, and
    /// drops symbols for files that disappeared.
    pub fn refresh(&mut self) -> Result<(Vec<Symbol>, RefreshStats)> {
        let stored = self.stored_file_stats()?;
        let mut seen: Vec<PathBuf> = Vec::new();
        let mut stats = RefreshStats::default();

        let tx = self.conn.transaction()?;
        {
            let walker = ignore::WalkBuilder::new(&self.root)
                .hidden(false)
                .require_git(false)
                .build();
            for entry in walker.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() || crate::Lang::from_path(path).is_none() {
                    continue;
                }
                let key = path.to_string_lossy().to_string();
                seen.push(path.to_path_buf());
                let size = meta.len() as i64;
                let mtime = mtime_secs(&meta);
                if let Some(&(s, m)) = stored.get(&key) {
                    if s == size && m == mtime {
                        stats.unchanged += 1;
                        continue; // unchanged: keep cached symbols, skip the parse
                    }
                }
                // New or changed: re-parse this one file, replace its rows.
                let symbols = parse_file(path).unwrap_or_default();
                tx.execute("DELETE FROM symbols WHERE file = ?1", [&key])?;
                Self::insert_symbols(&tx, &key, &symbols)?;
                tx.execute(
                    "INSERT INTO files (path, size, mtime) VALUES (?1, ?2, ?3)
                     ON CONFLICT(path) DO UPDATE SET size = ?2, mtime = ?3",
                    rusqlite::params![key, size, mtime],
                )?;
                stats.reparsed += 1;
            }

            // Files that vanished from the tree: drop their symbols.
            let seen_keys: std::collections::HashSet<String> = seen
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            for key in stored.keys() {
                if !seen_keys.contains(key) {
                    tx.execute("DELETE FROM symbols WHERE file = ?1", [key])?;
                    tx.execute("DELETE FROM files WHERE path = ?1", [key])?;
                    stats.deleted += 1;
                }
            }
        }
        tx.commit()?;

        Ok((self.load_symbols()?, stats))
    }

    /// Refresh and build the in-memory query graph.
    pub fn load_graph(&mut self) -> Result<Graph> {
        let (symbols, stats) = self.refresh()?;
        tracing::info!(
            "graph store: {} reparsed, {} unchanged, {} deleted -> {} symbols",
            stats.reparsed,
            stats.unchanged,
            stats.deleted,
            symbols.len()
        );
        Ok(Graph::from_symbols(symbols))
    }

    fn stored_file_stats(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT path, size, mtime FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (path, stat) = row?;
            out.insert(path, stat);
        }
        Ok(out)
    }

    fn insert_symbols(conn: &Connection, file: &str, symbols: &[Symbol]) -> Result<()> {
        let mut stmt = conn.prepare(
            "INSERT INTO symbols (file, name, kind, line, start_byte, end_byte, calls)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for s in symbols {
            let calls = serde_json::to_string(&s.calls)?;
            stmt.execute(rusqlite::params![
                file,
                s.name,
                s.kind.name(),
                s.line as i64,
                s.start_byte as i64,
                s.end_byte as i64,
                calls,
            ])?;
        }
        Ok(())
    }

    fn load_symbols(&self) -> Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, name, kind, line, start_byte, end_byte, calls FROM symbols ORDER BY file",
        )?;
        let rows = stmt.query_map([], |r| {
            let file: String = r.get(0)?;
            let name: String = r.get(1)?;
            let kind: String = r.get(2)?;
            let line: i64 = r.get(3)?;
            let start_byte: i64 = r.get(4)?;
            let end_byte: i64 = r.get(5)?;
            let calls: String = r.get(6)?;
            Ok((file, name, kind, line, start_byte, end_byte, calls))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (file, name, kind, line, start_byte, end_byte, calls) = row?;
            out.push(Symbol {
                name,
                kind: SymKind::from_name(&kind).unwrap_or(SymKind::Function),
                file: PathBuf::from(file),
                line: line as usize,
                start_byte: start_byte as usize,
                end_byte: end_byte as usize,
                calls: serde_json::from_str(&calls).unwrap_or_default(),
            });
        }
        Ok(out)
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A from-scratch full parse of `root` — the reference the incremental store is
/// validated against, and the toggle-off code path.
pub fn full_graph(root: &Path) -> Graph {
    Graph::from_symbols(parse_tree(root))
}
