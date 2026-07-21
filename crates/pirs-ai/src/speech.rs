//! OpenAI-compatible speech (STT + TTS) with multi-backend failover.
//!
//! Mirrors chat/embeddings: heavy models (Parakeet, Kokoro, Whisper, …) live
//! behind a **stateless** HTTP daemon speaking:
//!
//! - `POST {base}/audio/transcriptions` (multipart) → `{ "text": "…" }`
//! - `POST {base}/audio/speech` (JSON) → raw audio bytes
//!
//! Registry aliases with `caps = ["stt"]` / `["tts"]` and a `serve = [...]`
//! chain provide the same ordered failover as chat models.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::registry_file::{load_user_registry, BackendEntry, ModelEntry, RegistryFile};
use crate::{non_empty_env, AiError};

/// One concrete OpenAI-compatible speech endpoint + model id.
#[derive(Debug, Clone)]
pub struct SpeechEndpoint {
    pub backend_name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Options for transcription.
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub timeout: Option<Duration>,
}

/// Options for synthesis.
#[derive(Debug, Clone, Default)]
pub struct SpeakOptions {
    pub voice: Option<String>,
    /// e.g. `mp3`, `opus`, `aac`, `flac`, `wav`, `pcm` (OpenAI); daemons may accept `ogg`.
    pub response_format: Option<String>,
    pub speed: Option<f64>,
    pub timeout: Option<Duration>,
}

/// Client for a single speech backend (one base_url + model).
#[derive(Clone)]
pub struct SpeechClient {
    endpoint: SpeechEndpoint,
    client: reqwest::Client,
}

impl SpeechClient {
    pub fn new(endpoint: SpeechEndpoint) -> Self {
        SpeechClient {
            endpoint,
            client: reqwest::Client::builder()
                .user_agent(concat!("pirs/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    pub fn endpoint(&self) -> &SpeechEndpoint {
        &self.endpoint
    }

    pub async fn transcribe(
        &self,
        path: &Path,
        opts: &TranscribeOptions,
    ) -> Result<String, AiError> {
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|e| AiError::Stream(format!("read audio {}: {e}", path.display())))?;
        if bytes.is_empty() {
            return Err(AiError::Stream("audio file is empty".into()));
        }
        let raw_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("audio.ogg");
        // Telegram voice notes are often `*.oga` (Ogg/Opus). OpenAI/Groq type
        // gates list `ogg`/`opus` but not `oga` — normalize the upload name.
        let (filename, mime) = normalize_audio_upload_name(raw_name);

        let file_part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| AiError::Stream(format!("multipart mime: {e}")))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", self.endpoint.model.clone())
            .part("file", file_part);
        if let Some(lang) = &opts.language {
            form = form.text("language", lang.clone());
        }
        if let Some(prompt) = &opts.prompt {
            form = form.text("prompt", prompt.clone());
        }

        let url = format!(
            "{}/audio/transcriptions",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let timeout = opts.timeout.unwrap_or(Duration::from_secs(120));
        let mut req = self.client.post(&url).multipart(form).timeout(timeout);
        if let Some(key) = &self.endpoint.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AiError::Http {
                status: status.as_u16(),
                body,
            });
        }
        // Prefer JSON {text}; some daemons return plain text.
        if let Ok(v) = serde_json::from_str::<TranscriptionResponse>(&body) {
            let t = v.text.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
        let t = body.trim().to_string();
        if t.is_empty() {
            return Err(AiError::Decode("empty transcription".into()));
        }
        Ok(t)
    }

    pub async fn speak(&self, text: &str, opts: &SpeakOptions) -> Result<Vec<u8>, AiError> {
        if text.trim().is_empty() {
            return Err(AiError::Stream("empty TTS input".into()));
        }
        let url = format!(
            "{}/audio/speech",
            self.endpoint.base_url.trim_end_matches('/')
        );
        let mut body = json!({
            "model": self.endpoint.model,
            "input": text,
        });
        if let Some(v) = &opts.voice {
            body["voice"] = json!(v);
        }
        if let Some(f) = &opts.response_format {
            body["response_format"] = json!(f);
        }
        if let Some(s) = opts.speed {
            body["speed"] = json!(s);
        }
        let timeout = opts.timeout.unwrap_or(Duration::from_secs(120));
        let mut req = self.client.post(&url).json(&body).timeout(timeout);
        if let Some(key) = &self.endpoint.api_key {
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
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AiError::Decode(format!("tts body: {e}")))?;
        if bytes.is_empty() {
            return Err(AiError::Decode("empty tts audio".into()));
        }
        Ok(bytes.to_vec())
    }
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    #[serde(default)]
    text: String,
}

fn guess_audio_mime(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    // .oga = Ogg audio container (Telegram voice notes)
    if lower.ends_with(".ogg")
        || lower.ends_with(".oga")
        || lower.ends_with(".opus")
        || lower.ends_with(".ogx")
    {
        "audio/ogg"
    } else if lower.ends_with(".mp3") {
        "audio/mpeg"
    } else if lower.ends_with(".wav") {
        "audio/wav"
    } else if lower.ends_with(".webm") {
        "audio/webm"
    } else if lower.ends_with(".m4a") || lower.ends_with(".mp4") {
        "audio/mp4"
    } else if lower.ends_with(".flac") {
        "audio/flac"
    } else {
        "application/octet-stream"
    }
}

/// Filename + MIME for multipart upload (providers key off extension).
fn normalize_audio_upload_name(raw_name: &str) -> (String, &'static str) {
    let mime = guess_audio_mime(raw_name);
    let lower = raw_name.to_ascii_lowercase();
    let filename = if lower.ends_with(".oga") || lower.ends_with(".ogx") {
        // Groq/OpenAI accept "ogg"/"opus", not "oga".
        let stem = raw_name
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or("audio");
        format!("{stem}.ogg")
    } else if !raw_name.contains('.') {
        match mime {
            "audio/ogg" => format!("{raw_name}.ogg"),
            "audio/mpeg" => format!("{raw_name}.mp3"),
            "audio/wav" => format!("{raw_name}.wav"),
            "audio/webm" => format!("{raw_name}.webm"),
            _ => format!("{raw_name}.ogg"),
        }
    } else {
        raw_name.to_string()
    };
    (filename, mime)
}

// ─── Multi-backend resolution ───────────────────────────────────────────────

const CAP_STT: &str = "stt";
const CAP_TTS: &str = "tts";

/// Ordered endpoints for an STT or TTS alias (registry serve chain + env fallback).
#[derive(Debug, Clone)]
pub struct SpeechRoute {
    pub alias: String,
    pub kind: SpeechKind,
    pub endpoints: Vec<SpeechEndpoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechKind {
    Stt,
    Tts,
}

impl SpeechKind {
    fn cap(self) -> &'static str {
        match self {
            SpeechKind::Stt => CAP_STT,
            SpeechKind::Tts => CAP_TTS,
        }
    }

    fn env_model_keys(self) -> &'static [&'static str] {
        match self {
            SpeechKind::Stt => &["PIRS_STT_MODEL", "STT_MODEL"],
            SpeechKind::Tts => &["PIRS_TTS_MODEL", "TTS_MODEL"],
        }
    }
}

/// Resolve a speech route: explicit alias → env model → first registry model with cap
/// → pure env `PIRS_SPEECH_BASE_URL` / OpenAI audio fallback.
pub fn resolve_speech_route(kind: SpeechKind, alias: Option<&str>) -> SpeechRoute {
    let reg = load_user_registry();
    resolve_speech_route_in(&reg, kind, alias)
}

pub fn resolve_speech_route_in(
    reg: &RegistryFile,
    kind: SpeechKind,
    alias: Option<&str>,
) -> SpeechRoute {
    let cap = kind.cap();
    let wanted = alias
        .map(|s| s.to_string())
        .or_else(|| {
            kind.env_model_keys()
                .iter()
                .find_map(|k| non_empty_env(k))
        })
        .or_else(|| first_alias_with_cap(reg, cap));

    if let Some(name) = wanted {
        if let Some(mut eps) = endpoints_for_alias(reg, &name) {
            if !eps.is_empty() {
                // Append cloud/env backends not already in the serve chain
                // so a dead local daemon still fails over to Groq/OpenAI.
                append_unique_endpoints(&mut eps, &env_speech_endpoints(kind));
                return SpeechRoute {
                    alias: name,
                    kind,
                    endpoints: eps,
                };
            }
        }
        // Alias may be a raw remote model id — try env/cloud chain with that model.
        let eps = env_speech_endpoints_for_model(kind, Some(&name));
        if !eps.is_empty() {
            return SpeechRoute {
                alias: name,
                kind,
                endpoints: eps,
            };
        }
    }

    // Env/cloud chain only (no registry speech alias).
    let default_model = match kind {
        SpeechKind::Stt => non_empty_env("PIRS_STT_MODEL")
            .or_else(|| non_empty_env("STT_OPENAI_MODEL"))
            .or_else(|| non_empty_env("STT_GROQ_MODEL"))
            .unwrap_or_else(|| default_stt_model()),
        SpeechKind::Tts => non_empty_env("PIRS_TTS_MODEL")
            .or_else(|| non_empty_env("TTS_OPENAI_MODEL"))
            .unwrap_or_else(|| "tts-1".into()),
    };
    let endpoints = env_speech_endpoints_for_model(kind, Some(&default_model));
    SpeechRoute {
        alias: default_model,
        kind,
        endpoints,
    }
}

fn default_stt_model() -> String {
    // Prefer Groq's turbo when only GROQ_API_KEY is present.
    if non_empty_env("GROQ_API_KEY").is_some() && non_empty_env("OPENAI_API_KEY").is_none() {
        "whisper-large-v3-turbo".into()
    } else {
        "whisper-1".into()
    }
}

fn append_unique_endpoints(dst: &mut Vec<SpeechEndpoint>, extra: &[SpeechEndpoint]) {
    for ep in extra {
        // Dedupe by base+model so registry "groq-speech" and env "groq" don't double-call.
        let already = dst
            .iter()
            .any(|e| e.base_url == ep.base_url && e.model == ep.model);
        if !already {
            dst.push(ep.clone());
        }
    }
}

fn first_alias_with_cap(reg: &RegistryFile, cap: &str) -> Option<String> {
    reg.models
        .iter()
        .find(|m| m.caps.iter().any(|c| c.eq_ignore_ascii_case(cap)))
        .map(|m| m.alias.clone())
}

fn endpoints_for_alias(reg: &RegistryFile, alias: &str) -> Option<Vec<SpeechEndpoint>> {
    let model: &ModelEntry = reg.models.iter().find(|m| m.alias == alias)?;
    let mut out = Vec::new();
    for s in &model.serve {
        let Some(b) = reg.backends.iter().find(|b| b.name == s.backend) else {
            continue;
        };
        out.push(endpoint_from_backend(b, &s.model));
    }
    Some(out)
}

fn endpoint_from_backend(b: &BackendEntry, model: &str) -> SpeechEndpoint {
    let api_key = b
        .api_key_env
        .as_ref()
        .and_then(|e| non_empty_env(e));
    SpeechEndpoint {
        backend_name: b.name.clone(),
        base_url: b.base_url.trim_end_matches('/').to_string(),
        model: model.to_string(),
        api_key,
    }
}

/// Ordered env/cloud speech backends (local daemon → Groq → OpenAI).
pub fn env_speech_endpoints(kind: SpeechKind) -> Vec<SpeechEndpoint> {
    env_speech_endpoints_for_model(kind, None)
}

fn env_speech_endpoints_for_model(kind: SpeechKind, model_override: Option<&str>) -> Vec<SpeechEndpoint> {
    let mut out = Vec::new();

    // 1) Explicit local/custom OpenAI-compatible speech daemon.
    if let Some(base) = non_empty_env("PIRS_SPEECH_BASE_URL").or_else(|| non_empty_env("SPEECH_BASE_URL"))
    {
        let model = model_override
            .map(|s| s.to_string())
            .or_else(|| match kind {
                SpeechKind::Stt => non_empty_env("PIRS_STT_MODEL"),
                SpeechKind::Tts => non_empty_env("PIRS_TTS_MODEL"),
            })
            .unwrap_or_else(|| match kind {
                SpeechKind::Stt => "parakeet-tdt".into(),
                SpeechKind::Tts => "kokoro".into(),
            });
        let key = non_empty_env("PIRS_SPEECH_API_KEY").or_else(|| non_empty_env("OPENAI_API_KEY"));
        out.push(SpeechEndpoint {
            backend_name: "speech-local".into(),
            base_url: base.trim_end_matches('/').to_string(),
            model,
            api_key: key,
        });
    }

    // 2) Groq Whisper (STT only; free tier, OpenAI-compatible audio).
    if matches!(kind, SpeechKind::Stt) {
        if let Some(key) = non_empty_env("GROQ_API_KEY") {
            let model = model_override
                .filter(|m| {
                    // Don't force a Kokoro/Parakeet id onto Groq.
                    let m = m.to_ascii_lowercase();
                    m.contains("whisper") || m == "whisper-1"
                })
                .map(|s| s.to_string())
                .or_else(|| non_empty_env("STT_GROQ_MODEL"))
                .or_else(|| non_empty_env("PIRS_STT_MODEL").filter(|m| m.contains("whisper")))
                .unwrap_or_else(|| "whisper-large-v3-turbo".into());
            out.push(SpeechEndpoint {
                backend_name: "groq".into(),
                base_url: non_empty_env("GROQ_BASE_URL")
                    .unwrap_or_else(|| "https://api.groq.com/openai/v1".into())
                    .trim_end_matches('/')
                    .to_string(),
                model,
                api_key: Some(key),
            });
        }
    }

    // 3) OpenAI cloud STT/TTS.
    if let Some(key) = non_empty_env("OPENAI_API_KEY") {
        let model = match kind {
            SpeechKind::Stt => model_override
                .filter(|m| {
                    let m = m.to_ascii_lowercase();
                    m.contains("whisper") || m.contains("transcribe")
                })
                .map(|s| s.to_string())
                .or_else(|| non_empty_env("STT_OPENAI_MODEL"))
                .unwrap_or_else(|| "whisper-1".into()),
            SpeechKind::Tts => model_override
                .filter(|m| {
                    let m = m.to_ascii_lowercase();
                    m.contains("tts") || m.contains("gpt-4o")
                })
                .map(|s| s.to_string())
                .or_else(|| non_empty_env("TTS_OPENAI_MODEL"))
                .unwrap_or_else(|| "tts-1".into()),
        };
        out.push(SpeechEndpoint {
            backend_name: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            model,
            api_key: Some(key),
        });
    }

    out
}

/// Human-readable status of speech backends (for `pirs-claw speech status`).
/// Synchronous: no network probes (safe for unit tests / fast path).
pub fn speech_status_lines() -> Vec<String> {
    speech_status_lines_inner(None)
}

/// Status lines with optional live health probes (`GET {base}/health` or `/v1/health`).
pub async fn speech_status_lines_probed() -> Vec<String> {
    let reg = load_user_registry();
    let stt = resolve_speech_route_in(&reg, SpeechKind::Stt, None);
    let tts = resolve_speech_route_in(&reg, SpeechKind::Tts, None);
    let mut health: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut bases: Vec<String> = stt
        .endpoints
        .iter()
        .chain(tts.endpoints.iter())
        .map(|e| e.base_url.clone())
        .collect();
    bases.sort();
    bases.dedup();
    for base in bases {
        let h = probe_speech_base_health(&base).await;
        health.insert(base, h);
    }
    speech_status_lines_inner(Some(&health))
}

fn speech_status_lines_inner(health: Option<&std::collections::HashMap<String, String>>) -> Vec<String> {
    let reg = load_user_registry();
    let stt = resolve_speech_route_in(&reg, SpeechKind::Stt, None);
    let tts = resolve_speech_route_in(&reg, SpeechKind::Tts, None);
    let mut lines = Vec::new();
    lines.push(format!(
        "STT alias={} endpoints={}",
        stt.alias,
        stt.endpoints.len()
    ));
    for (i, ep) in stt.endpoints.iter().enumerate() {
        let key = if ep.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false) {
            "set"
        } else {
            "none"
        };
        let health_s = health
            .and_then(|h| h.get(&ep.base_url))
            .map(|s| format!(" health={s}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}. {} model={} base={} key={}{health_s}",
            i + 1,
            ep.backend_name,
            ep.model,
            ep.base_url,
            key
        ));
    }
    lines.push(format!(
        "TTS alias={} endpoints={}",
        tts.alias,
        tts.endpoints.len()
    ));
    for (i, ep) in tts.endpoints.iter().enumerate() {
        let key = if ep.api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false) {
            "set"
        } else {
            "none"
        };
        let health_s = health
            .and_then(|h| h.get(&ep.base_url))
            .map(|s| format!(" health={s}"))
            .unwrap_or_default();
        lines.push(format!(
            "  {}. {} model={} base={} key={}{health_s}",
            i + 1,
            ep.backend_name,
            ep.model,
            ep.base_url,
            key
        ));
    }
    if stt.endpoints.is_empty() {
        lines.push(
            "hint: no STT backends — run `pirs-claw speech setup --cloud` (uses GROQ/OPENAI keys) \
             or set PIRS_SPEECH_BASE_URL / install a local daemon"
                .into(),
        );
    }
    lines
}

/// Probe a speech OpenAI-compat base URL for liveness (1.5s timeout).
/// Tries `{base}/health` then parent `/health` for `.../v1` bases.
pub async fn probe_speech_base_health(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .connect_timeout(Duration::from_millis(800))
        .build()
    {
        Ok(c) => c,
        Err(_) => return "client-error".into(),
    };
    let candidates = [
        format!("{base}/health"),
        // strip trailing /v1
        base.strip_suffix("/v1")
            .map(|b| format!("{b}/health"))
            .unwrap_or_default(),
    ];
    for url in candidates {
        if url.is_empty() {
            continue;
        }
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body = resp.text().await.unwrap_or_default();
                let bl = body.to_ascii_lowercase();
                if bl.contains("mock") {
                    return "ok-mock".into();
                }
                if bl.contains("ok") || bl.contains("healthy") || body.trim().is_empty() {
                    return "ok".into();
                }
                return format!("ok({})", body.chars().take(40).collect::<String>().replace('\n', " "));
            }
            Ok(resp) => {
                // Cloud OpenAI often 404s on /health — treat as reachable-unknown.
                if resp.status().as_u16() == 404 {
                    return "reachable(no /health)".into();
                }
                return format!("http-{}", resp.status().as_u16());
            }
            Err(e) if e.is_timeout() => return "timeout".into(),
            Err(e) if e.is_connect() => return "unreachable".into(),
            Err(_) => continue,
        }
    }
    "unknown".into()
}

/// Default timeout for non-final STT/TTS backends in a failover chain (dead local daemons).
pub fn failover_attempt_timeout() -> Duration {
    std::env::var("PIRS_SPEECH_FAILOVER_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(12))
}

/// Final-backend timeout (real cloud STT/TTS can be slower).
pub fn final_attempt_timeout() -> Duration {
    std::env::var("PIRS_SPEECH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(120))
}

fn attempt_timeout(opts_timeout: Option<Duration>, is_last: bool) -> Duration {
    if let Some(t) = opts_timeout {
        return t;
    }
    if is_last {
        final_attempt_timeout()
    } else {
        failover_attempt_timeout()
    }
}

/// Transcribe with ordered failover across route endpoints.
pub async fn transcribe_with_failover(
    route: &SpeechRoute,
    path: &Path,
    opts: &TranscribeOptions,
) -> Result<(String, SpeechEndpoint), AiError> {
    if route.endpoints.is_empty() {
        return Err(AiError::Stream(
            "no STT backends configured (set PIRS_SPEECH_BASE_URL, OPENAI_API_KEY, \
             or [[models]] with caps=[\"stt\"] in ~/.pirs/config.toml)"
                .into(),
        ));
    }
    let mut last_err: Option<AiError> = None;
    let n = route.endpoints.len();
    for (i, ep) in route.endpoints.iter().enumerate() {
        let is_last = i + 1 >= n;
        let mut attempt = opts.clone();
        attempt.timeout = Some(attempt_timeout(opts.timeout, is_last));
        let client = SpeechClient::new(ep.clone());
        match client.transcribe(path, &attempt).await {
            Ok(text) => {
                // Mock/wiring transcripts must never block real backends when
                // any later endpoint remains (local pirs-audio mock → Groq).
                // Even as sole HTTP endpoint: still reject so CLI STT can run.
                if looks_like_mock_transcript(&text) {
                    if !is_last {
                        tracing::warn!(
                            backend = %ep.backend_name,
                            "stt got mock/wiring transcript; trying next backend"
                        );
                        last_err = Some(AiError::Stream(
                            "mock STT transcript skipped for failover".into(),
                        ));
                        continue;
                    }
                    // Last endpoint is mock: surface as error so voice CLI can try.
                    return Err(AiError::Stream(
                        "STT returned mock/wiring transcript only (no real backend succeeded)"
                            .into(),
                    ));
                }
                if i > 0 {
                    tracing::warn!(
                        backend = %ep.backend_name,
                        model = %ep.model,
                        "stt failover succeeded on endpoint {}",
                        i + 1
                    );
                }
                return Ok((text, ep.clone()));
            }
            Err(e) => {
                tracing::warn!(
                    backend = %ep.backend_name,
                    model = %ep.model,
                    error = %e,
                    "stt endpoint failed; trying next"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AiError::Stream("all STT backends failed".into())))
}

fn looks_like_mock_transcript(text: &str) -> bool {
    let t = text.trim();
    let tl = t.to_ascii_lowercase();
    t.starts_with("[pirs-audio mock STT]")
        || t.starts_with("mock transcript from pirs speech daemon")
        || tl.contains("[mock stt]")
        || tl.starts_with("mock stt:")
        || (tl.contains("pirs-audio") && tl.contains("mock"))
}

/// Heuristic: silent/near-empty TTS from mock engines (tiny WAV).
fn looks_like_mock_or_empty_tts(audio: &[u8]) -> bool {
    if audio.len() < 64 {
        return true;
    }
    // Standard mock silent WAV is often a few KB of near-zero PCM after RIFF header.
    if audio.len() < 4096 && audio.starts_with(b"RIFF") {
        let nonzero = audio.iter().skip(44).filter(|&&b| b != 0).count();
        if nonzero < 16 {
            return true;
        }
    }
    false
}

/// Synthesize with ordered failover.
pub async fn speak_with_failover(
    route: &SpeechRoute,
    text: &str,
    opts: &SpeakOptions,
) -> Result<(Vec<u8>, SpeechEndpoint), AiError> {
    if route.endpoints.is_empty() {
        return Err(AiError::Stream(
            "no TTS backends configured (set PIRS_SPEECH_BASE_URL, OPENAI_API_KEY, \
             or [[models]] with caps=[\"tts\"] in ~/.pirs/config.toml)"
                .into(),
        ));
    }
    let mut last_err: Option<AiError> = None;
    let n = route.endpoints.len();
    for (i, ep) in route.endpoints.iter().enumerate() {
        let is_last = i + 1 >= n;
        let mut attempt = opts.clone();
        attempt.timeout = Some(attempt_timeout(opts.timeout, is_last));
        let client = SpeechClient::new(ep.clone());
        match client.speak(text, &attempt).await {
            Ok(audio) => {
                if looks_like_mock_or_empty_tts(&audio) && !is_last {
                    tracing::warn!(
                        backend = %ep.backend_name,
                        bytes = audio.len(),
                        "tts looks empty/mock; trying next backend"
                    );
                    last_err = Some(AiError::Stream("mock/empty TTS skipped".into()));
                    continue;
                }
                if looks_like_mock_or_empty_tts(&audio) && is_last {
                    return Err(AiError::Stream(
                        "TTS returned empty/mock audio only (no real backend succeeded)".into(),
                    ));
                }
                if i > 0 {
                    tracing::warn!(
                        backend = %ep.backend_name,
                        model = %ep.model,
                        "tts failover succeeded on endpoint {}",
                        i + 1
                    );
                }
                return Ok((audio, ep.clone()));
            }
            Err(e) => {
                tracing::warn!(
                    backend = %ep.backend_name,
                    model = %ep.model,
                    error = %e,
                    "tts endpoint failed; trying next"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AiError::Stream("all TTS backends failed".into())))
}

/// Convenience: resolve default STT route and transcribe.
pub async fn transcribe_path(
    path: &Path,
    alias: Option<&str>,
    opts: &TranscribeOptions,
) -> Result<(String, SpeechEndpoint), AiError> {
    let route = resolve_speech_route(SpeechKind::Stt, alias);
    transcribe_with_failover(&route, path, opts).await
}

/// Convenience: resolve default TTS route and speak.
pub async fn speak_text(
    text: &str,
    alias: Option<&str>,
    opts: &SpeakOptions,
) -> Result<(Vec<u8>, SpeechEndpoint), AiError> {
    let route = resolve_speech_route(SpeechKind::Tts, alias);
    speak_with_failover(&route, text, opts).await
}

/// Write audio bytes to a temp-ish path under `dir`.
pub fn write_audio_file(dir: &Path, bytes: &[u8], ext: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let name = format!(
        "tts_{}.{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        ext.trim_start_matches('.')
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry_file::parse_from_config_value;

    fn sample_reg() -> RegistryFile {
        let sample = r#"
[[backends]]
name = "speech-local"
kind = "openai_compatible"
base_url = "http://127.0.0.1:8090/v1"

[[backends]]
name = "openai"
kind = "openai_compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[models]]
alias = "stt-default"
caps = ["stt"]
serve = [
  { backend = "speech-local", model = "parakeet-tdt" },
  { backend = "openai", model = "whisper-1" },
]

[[models]]
alias = "tts-default"
caps = ["tts"]
serve = [
  { backend = "speech-local", model = "kokoro" },
  { backend = "openai", model = "tts-1" },
]
"#;
        parse_from_config_value(&sample.parse().unwrap())
    }

    #[test]
    fn stt_route_orders_failover_chain() {
        let reg = sample_reg();
        let route = resolve_speech_route_in(&reg, SpeechKind::Stt, Some("stt-default"));
        // Registry serve list first; env/cloud backends may be appended after.
        assert!(route.endpoints.len() >= 2, "{:?}", route.endpoints);
        assert_eq!(route.endpoints[0].backend_name, "speech-local");
        assert_eq!(route.endpoints[0].model, "parakeet-tdt");
        assert_eq!(route.endpoints[1].backend_name, "openai");
        assert_eq!(route.endpoints[1].model, "whisper-1");
    }

    #[test]
    fn tts_route_from_cap_when_no_alias() {
        let reg = sample_reg();
        let route = resolve_speech_route_in(&reg, SpeechKind::Tts, None);
        assert_eq!(route.alias, "tts-default");
        assert_eq!(route.endpoints[0].model, "kokoro");
    }

    #[test]
    fn guess_mime_ogg() {
        assert_eq!(guess_audio_mime("a.ogg"), "audio/ogg");
        assert_eq!(guess_audio_mime("a.oga"), "audio/ogg");
        assert_eq!(guess_audio_mime("a.mp3"), "audio/mpeg");
        let (name, mime) = normalize_audio_upload_name("file_0.oga");
        assert_eq!(name, "file_0.ogg");
        assert_eq!(mime, "audio/ogg");
    }

    #[test]
    fn env_chain_includes_groq_when_key_set() {
        // Avoid clobbering developer env permanently: only assert pure helper shape
        // when GROQ is already present in this process.
        if non_empty_env("GROQ_API_KEY").is_some() {
            let eps = env_speech_endpoints(SpeechKind::Stt);
            assert!(
                eps.iter().any(|e| e.backend_name == "groq"),
                "expected groq in {eps:?}"
            );
            assert!(eps.iter().any(|e| e.model.contains("whisper")));
        }
    }

    #[test]
    fn mock_transcript_detector() {
        assert!(looks_like_mock_transcript(
            "[pirs-audio mock STT] hello from wiring"
        ));
        assert!(looks_like_mock_transcript(
            "Mock STT: pirs-audio placeholder"
        ));
        assert!(!looks_like_mock_transcript(
            "real user said remember the milk"
        ));
    }

    #[test]
    fn mock_tts_detector_tiny_wav() {
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0u8; 100]);
        assert!(looks_like_mock_or_empty_tts(&wav));
        assert!(looks_like_mock_or_empty_tts(&[1, 2, 3]));
        assert!(!looks_like_mock_or_empty_tts(&vec![7u8; 8000]));
    }

    #[test]
    fn attempt_timeout_short_then_long() {
        assert!(attempt_timeout(None, false) <= Duration::from_secs(30));
        assert!(attempt_timeout(None, true) >= Duration::from_secs(30));
        assert_eq!(
            attempt_timeout(Some(Duration::from_secs(3)), false),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn status_lines_without_probe_list_endpoints() {
        let lines = speech_status_lines();
        assert!(lines.iter().any(|l| l.starts_with("STT ")));
        assert!(lines.iter().any(|l| l.starts_with("TTS ")));
    }
}
