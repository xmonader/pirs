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
    /// Open (or create) the store at `db_path`.
    ///
    /// Only wipe on **schema/version** incompatibility — never on transient
    /// `SQLITE_BUSY` / lock contention (review M-22).
    pub fn open(db_path: &Path, root: &Path) -> Result<GraphStore> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = match Self::open_verified(db_path) {
            Ok(conn) => conn,
            Err(e) => {
                let msg = e.to_string();
                let busy = msg.to_ascii_lowercase().contains("busy")
                    || msg.to_ascii_lowercase().contains("locked");
                if busy {
                    return Err(e).with_context(|| {
                        format!(
                            "graph store busy at {} (another process indexing?); not deleting",
                            db_path.display()
                        )
                    });
                }
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
        // Wait briefly under concurrent readers instead of failing instantly.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
             CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
             CREATE TABLE IF NOT EXISTS embeddings (
                 file TEXT NOT NULL, start_byte INTEGER NOT NULL, name TEXT NOT NULL,
                 line INTEGER NOT NULL, vector BLOB NOT NULL,
                 PRIMARY KEY (file, start_byte));",
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
                // New or changed: re-parse this one file, replace its rows. Its
                // embeddings are now stale (byte offsets shifted) — drop them so
                // they get re-embedded against the new content.
                let symbols = parse_file(path).unwrap_or_default();
                tx.execute("DELETE FROM symbols WHERE file = ?1", [&key])?;
                tx.execute("DELETE FROM embeddings WHERE file = ?1", [&key])?;
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
                    tx.execute("DELETE FROM embeddings WHERE file = ?1", [key])?;
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

    // ---- Semantic (embedding) layer -------------------------------------------------
    //
    // The store never talks to the network. The caller embeds text via
    // `EmbeddingClient` and hands vectors back here, so this half stays sync and
    // testable with a fake embedder. Vectors live in one model's space only, so
    // `ensure_model` is the guard that drops the whole embedding set the instant
    // the configured model/dim changes — a silent swap otherwise makes cosine
    // search return confident garbage.

    /// Reconcile the stored embedding space with `(model, dim)`. If it differs
    /// from what produced the current vectors (or there were none), every vector
    /// is dropped and the stamp updated; returns `true` when a wipe happened so
    /// the caller knows a full re-embed is due.
    pub fn ensure_model(&mut self, model: &str, dim: usize) -> Result<bool> {
        let cur_model: Option<String> = self.meta_get("embed_model")?;
        let cur_dim: Option<String> = self.meta_get("embed_dim")?;
        let matches =
            cur_model.as_deref() == Some(model) && cur_dim.as_deref() == Some(&dim.to_string());
        if matches {
            return Ok(false);
        }
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM embeddings", [])?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('embed_model', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [model],
        )?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('embed_dim', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [dim.to_string()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Symbols that have no vector yet, with the text to embed (kind + name +
    /// source body, truncated to `max_chars` so a giant function can't blow the
    /// model's token limit). This is the batch the caller sends to the embedder.
    pub fn pending_embeddings(&self, max_chars: usize) -> Result<Vec<EmbedItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.file, s.name, s.kind, s.line, s.start_byte, s.end_byte
             FROM symbols s
             LEFT JOIN embeddings e ON s.file = e.file AND s.start_byte = e.start_byte
             WHERE e.vector IS NULL",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut file_cache: HashMap<String, String> = HashMap::new();
        let mut out = Vec::new();
        for row in rows {
            let (file, name, kind, line, start_byte, end_byte) = row?;
            let source = file_cache
                .entry(file.clone())
                .or_insert_with(|| std::fs::read_to_string(&file).unwrap_or_default());
            let body = source
                .get(start_byte as usize..end_byte as usize)
                .unwrap_or("");
            let text: String = format!("{kind} {name}\n{body}")
                .chars()
                .take(max_chars)
                .collect();
            out.push(EmbedItem {
                file: PathBuf::from(file),
                start_byte: start_byte as usize,
                name,
                line: line as usize,
                text,
            });
        }
        Ok(out)
    }

    /// Persist `vectors` (parallel to `items`) as the embeddings for those
    /// symbols. Length mismatch is a hard error — a misaligned batch would map
    /// vectors to the wrong symbols.
    pub fn store_embeddings(&mut self, items: &[EmbedItem], vectors: &[Vec<f32>]) -> Result<()> {
        if items.len() != vectors.len() {
            anyhow::bail!(
                "embedding batch mismatch: {} items vs {} vectors",
                items.len(),
                vectors.len()
            );
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO embeddings (file, start_byte, name, line, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(file, start_byte) DO UPDATE SET vector = ?5, name = ?3, line = ?4",
            )?;
            for (item, vec) in items.iter().zip(vectors.iter()) {
                stmt.execute(rusqlite::params![
                    item.file.to_string_lossy(),
                    item.start_byte as i64,
                    item.name,
                    item.line as i64,
                    vec_to_blob(vec),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Top-`k` symbols by cosine similarity to `query`. Brute-force over every
    /// stored vector — fine into the hundreds of thousands; add an ANN index only
    /// past that. Vectors of a different length than the query score 0.0.
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<SemanticHit>> {
        let mut stmt = self
            .conn
            .prepare("SELECT file, name, line, vector FROM embeddings")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        let mut scored: Vec<SemanticHit> = Vec::new();
        for row in rows {
            let (file, name, line, blob) = row?;
            let vec = blob_to_vec(&blob);
            let score = pirs_ai::cosine(query, &vec);
            scored.push(SemanticHit {
                file: PathBuf::from(file),
                name,
                line: line as usize,
                score,
            });
        }
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(k);
        Ok(scored)
    }

    pub fn embedding_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok())
    }
}

/// A symbol awaiting embedding: its identity plus the text to send the embedder.
#[derive(Debug, Clone)]
pub struct EmbedItem {
    pub file: PathBuf,
    pub start_byte: usize,
    pub name: String,
    pub line: usize,
    pub text: String,
}

/// A semantic-search result: where the symbol is and how close it scored.
#[derive(Debug, Clone)]
pub struct SemanticHit {
    pub file: PathBuf,
    pub name: String,
    pub line: usize,
    pub score: f32,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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
