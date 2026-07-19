//! Background embedding indexer.
//!
//! Cold-embedding a repo can take many minutes (a 768-dim model on CPU embeds
//! ~2 code chunks/sec), which must never block a search or be lost on a restart.
//! This task fills the semantic index *behind* BM25: `code_search` answers from
//! lexical + graph immediately, and semantic hits light up as vectors land.
//!
//! Two properties make it product-grade rather than a prototype:
//!   - **Checkpointed**: every batch is persisted before the next starts, so a
//!     kill/restart resumes from the last batch instead of losing everything.
//!   - **Self-healing**: it re-refreshes the graph on an interval, so edits made
//!     while it runs get re-embedded; transient service errors are logged and
//!     retried, never fatal (the index is an aid, not a hard dependency).

use std::path::PathBuf;
use std::time::Duration;

use pirs_ai::EmbeddingClient;

use crate::embed_util::embed_batch;
use crate::store::{EmbedItem, GraphStore};

/// Symbols embedded (and checkpointed) per iteration.
const CHECKPOINT_BATCH: usize = 64;
/// How long to wait before re-checking for pending work once caught up, and the
/// retry backoff when the embedding service is unreachable.
const IDLE_RECHECK: Duration = Duration::from_secs(30);

/// Owns the inputs needed to keep the embedding index caught up with the graph.
pub struct BackgroundIndexer {
    root: PathBuf,
    db_path: PathBuf,
    embedder: EmbeddingClient,
    max_chars: usize,
}

impl BackgroundIndexer {
    pub fn new(
        root: PathBuf,
        db_path: PathBuf,
        embedder: EmbeddingClient,
        max_chars: usize,
    ) -> Self {
        BackgroundIndexer {
            root,
            db_path,
            embedder,
            max_chars,
        }
    }

    /// Run until cancelled/dropped: keep embedding pending symbols in
    /// checkpointed batches, sleeping when the index is fully caught up.
    pub async fn run(self) {
        let dim = match self.learn_dim().await {
            Some(d) => d,
            None => return, // cancelled before the service ever came up
        };
        tracing::info!(
            "bg embedding indexer started (model={}, dim={dim})",
            self.embedder.model()
        );
        loop {
            match self.tick(dim).await {
                Ok(0) => tokio::time::sleep(IDLE_RECHECK).await, // caught up; poll for edits
                Ok(_) => tokio::task::yield_now().await,         // did work; grab the next batch
                Err(e) => {
                    tracing::warn!("bg index tick failed: {e:#}");
                    tokio::time::sleep(IDLE_RECHECK).await;
                }
            }
        }
    }

    /// Probe the service once to learn the vector dimension, retrying on the idle
    /// interval until it answers (it may not be up when the process starts).
    async fn learn_dim(&self) -> Option<usize> {
        loop {
            match self
                .embedder
                .embed(std::slice::from_ref(&"dimension probe".to_string()))
                .await
            {
                Ok(v) if v.first().is_some_and(|x| !x.is_empty()) => return Some(v[0].len()),
                Ok(_) => tracing::warn!("embed probe returned no vector; retrying"),
                Err(e) => tracing::warn!("embed service not ready ({e:#}); retrying"),
            }
            tokio::time::sleep(IDLE_RECHECK).await;
        }
    }

    /// One unit of work: refresh the graph, embed the next batch of pending
    /// symbols, and checkpoint it. Returns how many symbols were pending
    /// (0 means the index is fully caught up). The DB connection is never held
    /// across the `.await` on the embedding service.
    async fn tick(&self, dim: usize) -> anyhow::Result<usize> {
        let pending: Vec<EmbedItem> = {
            let mut store = GraphStore::open(&self.db_path, &self.root)?;
            store.refresh()?;
            store.ensure_model(self.embedder.model(), dim)?;
            let mut p = store.pending_embeddings(self.max_chars)?;
            p.truncate(CHECKPOINT_BATCH);
            p
        };
        if pending.is_empty() {
            return Ok(0);
        }
        let total_pending = pending.len();

        let (kept_idx, vecs) = embed_batch(&self.embedder, &pending).await;
        let kept: Vec<EmbedItem> = kept_idx.iter().map(|&i| pending[i].clone()).collect();

        let mut store = GraphStore::open(&self.db_path, &self.root)?;
        store.store_embeddings(&kept, &vecs)?;
        tracing::info!(
            "bg index: +{} embedded (total {})",
            kept.len(),
            store.embedding_count().unwrap_or(0)
        );
        Ok(total_pending)
    }
}
