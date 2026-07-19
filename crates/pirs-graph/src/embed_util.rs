//! Shared embedding helpers used by both the on-demand `code_search` top-up and
//! the background indexer. Batches inputs, and on a batch failure falls back to
//! per-item embedding with progressive truncation — so one dense chunk that
//! exceeds a small-context model can't abort a whole batch.

use pirs_ai::EmbeddingClient;

use crate::store::EmbedItem;

/// Symbols per embedding request.
pub(crate) const EMBED_BATCH: usize = 64;

/// Embed `items`, returning `(kept_indices, vectors)` aligned by position:
/// `kept_indices[j]` is the index into `items` that produced `vectors[j]`.
/// Items that can't be embedded even after truncation are simply dropped.
pub(crate) async fn embed_batch(
    embedder: &EmbeddingClient,
    items: &[EmbedItem],
) -> (Vec<usize>, Vec<Vec<f32>>) {
    let mut idxs = Vec::new();
    let mut vecs = Vec::new();
    for (b, chunk) in items.chunks(EMBED_BATCH).enumerate() {
        let base = b * EMBED_BATCH;
        let texts: Vec<String> = chunk.iter().map(|i| i.text.clone()).collect();
        match embedder.embed(&texts).await {
            Ok(v) => {
                for (j, vec) in v.into_iter().enumerate() {
                    idxs.push(base + j);
                    vecs.push(vec);
                }
            }
            Err(_) => {
                for (j, item) in chunk.iter().enumerate() {
                    if let Some(vec) = embed_one(embedder, &item.text).await {
                        idxs.push(base + j);
                        vecs.push(vec);
                    }
                }
            }
        }
    }
    (idxs, vecs)
}

/// Embed a single text, halving it on failure until it fits the model's context
/// (or giving up once it's too small to be worth embedding).
pub(crate) async fn embed_one(embedder: &EmbeddingClient, text: &str) -> Option<Vec<f32>> {
    let mut t = text.to_string();
    for _ in 0..6 {
        match embedder.embed(std::slice::from_ref(&t)).await {
            Ok(mut v) => return v.pop(),
            Err(_) => {
                let n = t.chars().count();
                if n <= 64 {
                    return None;
                }
                t = t.chars().take(n / 2).collect();
            }
        }
    }
    None
}
