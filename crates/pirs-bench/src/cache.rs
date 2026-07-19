//! SHA-keyed baseline cache.
//!
//! Capturing a stable baseline means running the set **twice** — the single
//! most expensive repeated cost in a run, and identical every time the repo sits
//! at the same commit. So we key per-test outcomes by `(base_sha, test_id)` and
//! reuse them: a second task (or a retried attempt) at the same checkout pays
//! nothing for tests already seen.
//!
//! **Validity contract:** the key is the commit SHA, so the cache is only sound
//! when the *environment* is deterministic per SHA — the SWE-bench model (one
//! fixed container per instance). It is therefore opt-in: no SHA, no caching. A
//! caller that mutates the environment between reads at the same SHA must not
//! share a cache across that boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::types::{TestId, TestOutcome};

/// A persistable map of `(base_sha, test_id) -> stable baseline outcome`.
#[derive(Default)]
pub struct BaselineCache {
    /// Backing file, if this cache persists. In-memory only when `None`.
    path: Option<PathBuf>,
    /// Keyed by [`key`]; the value is a stable (twice-agreed) outcome.
    entries: HashMap<String, TestOutcome>,
    dirty: bool,
}

/// The composite key. NUL can't appear in a git SHA or a test id, so it is an
/// unambiguous separator.
fn key(sha: &str, id: &str) -> String {
    format!("{sha}\0{id}")
}

impl BaselineCache {
    /// An in-memory-only cache (never persisted).
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load (or start) a file-backed cache. A missing file is not an error — it
    /// means an empty cache that will be created on first [`save`](Self::save).
    /// A corrupt file is logged and treated as empty rather than aborting a run.
    pub fn load(path: &Path) -> Self {
        let entries = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, TestOutcome>>(&bytes)
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        "baseline cache {} is corrupt, ignoring: {e}",
                        path.display()
                    );
                    HashMap::new()
                }),
            Err(_) => HashMap::new(),
        };
        BaselineCache {
            path: Some(path.to_path_buf()),
            entries,
            dirty: false,
        }
    }

    /// The cached stable outcome for a test at a checkout, if known.
    pub fn get(&self, sha: &str, id: &str) -> Option<TestOutcome> {
        self.entries.get(&key(sha, id)).copied()
    }

    /// Record a stable outcome. Overwrites any prior value for the same key
    /// (a re-observed outcome at the same SHA should agree; last write wins).
    pub fn put(&mut self, sha: &str, id: &str, outcome: TestOutcome) {
        let k = key(sha, id);
        if self.entries.get(&k) != Some(&outcome) {
            self.entries.insert(k, outcome);
            self.dirty = true;
        }
    }

    /// Persist to the backing file if dirty. No-op for an in-memory cache or when
    /// nothing changed. Writes atomically (temp file + rename) so a crash mid-write
    /// can't leave a truncated, corrupt cache.
    pub fn save(&mut self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.dirty {
            return Ok(());
        }
        let bytes = serde_json::to_vec(&self.entries).context("serialize baseline cache")?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
        self.dirty = false;
        Ok(())
    }

    /// Partition `ids` into (cached outcomes, ids still needing a run) at `sha`.
    pub fn split(&self, sha: &str, ids: &[TestId]) -> (Vec<(TestId, TestOutcome)>, Vec<TestId>) {
        let mut hits = Vec::new();
        let mut misses = Vec::new();
        for id in ids {
            match self.get(sha, id) {
                Some(o) => hits.push((id.clone(), o)),
                None => misses.push(id.clone()),
            }
        }
        (hits, misses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;

    #[test]
    fn put_get_roundtrip_is_sha_scoped() {
        let mut c = BaselineCache::in_memory();
        c.put("sha1", "t", Fail);
        assert_eq!(c.get("sha1", "t"), Some(Fail));
        // Different SHA is a different key — no bleed-through.
        assert_eq!(c.get("sha2", "t"), None);
    }

    #[test]
    fn split_separates_hits_from_misses() {
        let mut c = BaselineCache::in_memory();
        c.put("s", "a", Pass);
        let ids = vec!["a".to_string(), "b".to_string()];
        let (hits, misses) = c.split("s", &ids);
        assert_eq!(hits, vec![("a".to_string(), Pass)]);
        assert_eq!(misses, vec!["b".to_string()]);
    }

    #[test]
    fn persists_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        {
            let mut c = BaselineCache::load(&path);
            c.put("sha", "t1", Fail);
            c.put("sha", "t2", Pass);
            c.save().unwrap();
        }
        let c2 = BaselineCache::load(&path);
        assert_eq!(c2.get("sha", "t1"), Some(Fail));
        assert_eq!(c2.get("sha", "t2"), Some(Pass));
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        std::fs::write(&path, b"{not json").unwrap();
        let c = BaselineCache::load(&path);
        assert_eq!(c.get("sha", "t"), None);
    }

    #[test]
    fn save_is_noop_when_not_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("baseline.json");
        let mut c = BaselineCache::load(&path);
        c.save().unwrap(); // nothing written
        assert!(!path.exists(), "clean cache must not create a file");
    }
}
