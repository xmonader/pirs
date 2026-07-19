//! A minimal client for an OpenAI-compatible `/v1/embeddings` endpoint.
//!
//! Embeddings are treated exactly like the chat provider: point pirs at any
//! server that speaks `/embeddings` — Ollama, LM Studio, llama.cpp, HuggingFace
//! TEI, a fastembed sidecar, or a cloud API — via a base URL, model name, and
//! optional API key. No native ONNX/`ort` dependency lives in pirs; the heavy
//! inference sits behind this HTTP boundary.
//!
//! Vectors are only comparable within one model's space, so callers MUST record
//! [`EmbeddingClient::model`] alongside every stored vector and re-embed when it
//! changes — a silent model swap otherwise corrupts similarity search with no
//! error. See `pirs-graph`'s store for that guard.

use serde_json::json;

use crate::AiError;

/// Client for a single embedding model behind an OpenAI-compatible endpoint.
#[derive(Clone)]
pub struct EmbeddingClient {
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl EmbeddingClient {
    /// `base_url` is the OpenAI-compatible root (e.g. `http://localhost:11434/v1`
    /// for Ollama); `/embeddings` is appended, mirroring the chat client's
    /// `/chat/completions`.
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        EmbeddingClient {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: model.into(),
            api_key,
            client: reqwest::Client::builder()
                .user_agent(concat!("pirs/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// The model id, to be stamped on every stored vector.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Embed a batch of inputs, returning one vector per input in input order.
    /// Empty input returns empty without a request.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, AiError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", self.base_url);
        let mut req = self
            .client
            .post(&url)
            .json(&json!({ "model": self.model, "input": inputs }));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AiError::Http {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: EmbeddingResponse = resp
            .json()
            .await
            .map_err(|e| AiError::Decode(format!("embeddings response: {e}")))?;

        // Respect the `index` field so we never mis-align a vector with its input
        // even if the server returns them out of order.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);
        if data.len() != inputs.len() {
            return Err(AiError::Decode(format!(
                "expected {} embeddings, got {}",
                inputs.len(),
                data.len()
            )));
        }
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(serde::Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// Cosine similarity of two equal-length vectors. Returns 0.0 on a length
/// mismatch or a zero-magnitude vector rather than NaN, so a bad vector can
/// never poison a ranking.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one_orthogonal_is_zero() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine(&a, &c).abs() < 1e-6);
    }

    #[test]
    fn cosine_guards_length_and_zero() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}
