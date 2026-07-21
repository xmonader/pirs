//! Minimal OpenAI-compatible HTTP surface.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::engines::{self, EngineConfig, SttEngine, TtsEngine};
use crate::ffmpeg;

pub struct AppState {
    pub stt: Box<dyn SttEngine>,
    pub tts: Box<dyn TtsEngine>,
    pub stt_model_id: String,
    pub tts_model_id: String,
}

pub async fn serve(host: &str, port: u16, cfg: EngineConfig) -> anyhow::Result<()> {
    let stt = engines::select_stt(&cfg)?;
    let tts = engines::select_tts(&cfg)?;
    tracing::info!(
        stt = stt.name(),
        tts = tts.name(),
        "pirs-audio engines selected"
    );
    let state = Arc::new(AppState {
        stt,
        tts,
        stt_model_id: cfg.stt_model_id,
        tts_model_id: cfg.tts_model_id,
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/health", get(health))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/v1/audio/transcriptions", post(transcribe))
        .route("/audio/transcriptions", post(transcribe))
        .route("/v1/audio/speech", post(speech))
        .route("/audio/speech", post(speech))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    tracing::info!("pirs-audio listening on http://{addr}/v1");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "stt": state.stt.name(),
        "tts": state.tts.name(),
        "impl": "rust",
    }))
}

async fn models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "object": "list",
        "data": [
            {
                "id": state.stt_model_id,
                "object": "model",
                "owned_by": "pirs-audio",
            },
            {
                "id": state.tts_model_id,
                "object": "model",
                "owned_by": "pirs-audio",
            },
            {
                "id": format!("stt:{}", state.stt.name()),
                "object": "model",
                "owned_by": "pirs-audio",
            },
            {
                "id": format!("tts:{}", state.tts.name()),
                "object": "model",
                "owned_by": "pirs-audio",
            },
        ]
    }))
}

async fn transcribe(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Result<impl IntoResponse, ApiError> {
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut filename = "audio.ogg".to_string();
    let mut language: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad(format!("multipart: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                if let Some(fn_) = field.file_name() {
                    filename = fn_.to_string();
                }
                file_bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| ApiError::bad(format!("read file: {e}")))?
                        .to_vec(),
                );
            }
            "language" => {
                language = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::bad(format!("language: {e}")))?,
                );
            }
            "model" => {
                let _ = field.bytes().await; // ignore — we use configured engine
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes.ok_or_else(|| ApiError::bad("missing file field"))?;
    if bytes.is_empty() {
        return Err(ApiError::bad("empty file"));
    }

    // Normalize Telegram .oga → .ogg for tools that key off extension.
    let mut safe_name = filename;
    if safe_name.to_ascii_lowercase().ends_with(".oga") {
        safe_name = safe_name[..safe_name.len() - 4].to_string() + ".ogg";
    }

    let dir = tempfile::tempdir().map_err(|e| ApiError::server(e.to_string()))?;
    let path = dir.path().join(
        std::path::Path::new(&safe_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("audio.ogg")),
    );
    std::fs::write(&path, &bytes).map_err(|e| ApiError::server(e.to_string()))?;
    let wav = ffmpeg::ensure_wav(&path).map_err(|e| ApiError::server(e.to_string()))?;

    let stt = Arc::clone(&state);
    let lang = language.clone();
    let path_for_job = wav.clone();
    let text = tokio::task::spawn_blocking(move || stt.stt.transcribe(&path_for_job, lang.as_deref()))
        .await
        .map_err(|e| ApiError::server(format!("join: {e}")))?
        .map_err(|e| ApiError::server(e.to_string()))?;

    Ok(Json(json!({ "text": text })))
}

#[derive(Debug, Deserialize)]
struct SpeechBody {
    input: Option<String>,
    text: Option<String>,
    voice: Option<String>,
    response_format: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
}

async fn speech(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SpeechBody>,
) -> Result<Response, ApiError> {
    let text = body
        .input
        .or(body.text)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return Err(ApiError::bad("missing input"));
    }
    let format = body
        .response_format
        .unwrap_or_else(|| "wav".into())
        .to_ascii_lowercase();
    let voice = body.voice;
    let format_for_job = format.clone();

    let tts = Arc::clone(&state);
    let audio = tokio::task::spawn_blocking(move || {
        tts.tts.speak(&text, voice.as_deref(), &format_for_job)
    })
    .await
    .map_err(|e| ApiError::server(format!("join: {e}")))?
    .map_err(|e| ApiError::server(e.to_string()))?;

    let ct = match format.as_str() {
        "mp3" => "audio/mpeg",
        "opus" | "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        _ => "audio/wav",
    };
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, ct)],
        audio,
    )
        .into_response())
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    fn server(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": {
                "message": self.message,
                "type": if self.status.is_client_error() {
                    "invalid_request_error"
                } else {
                    "server_error"
                }
            }
        });
        (self.status, Json(body)).into_response()
    }
}
