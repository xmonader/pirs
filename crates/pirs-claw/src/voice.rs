//! Voice memo transcription + TTS (OpenAI-compatible multi-backend).
//!
//! STT resolution order:
//! 1. Registry aliases with `caps = ["stt"]` (or `PIRS_STT_MODEL`) — serve-chain failover
//! 2. `PIRS_SPEECH_BASE_URL` / `OPENAI_API_KEY` cloud audio
//! 3. `PIRS_CLAW_TRANSCRIBE_CMD` shell template
//! 4. Local `whisper` / `whisper-cpp` / `faster-whisper` CLIs
//!
//! TTS (gateway replies):
//! - `PIRS_CLAW_TTS=1` — attach voice to every reply when TTS backends exist
//! - `PIRS_CLAW_TTS_ON_VOICE=1` — voice reply only when the user sent a voice note

use std::path::Path;
use std::process::Command;

use pirs_ai::{
    resolve_speech_route, speak_with_failover, transcribe_with_failover, SpeakOptions, SpeechEndpoint,
    SpeechKind, TranscribeOptions,
};

/// Try to transcribe an audio file to text (async; prefers HTTP backends).
pub async fn transcribe_audio(path: &Path) -> anyhow::Result<Option<String>> {
    if !path.is_file() {
        anyhow::bail!("audio file not found: {}", path.display());
    }

    let route = resolve_speech_route(SpeechKind::Stt, None);
    if !route.endpoints.is_empty() {
        match transcribe_with_failover(&route, path, &TranscribeOptions::default()).await {
            Ok((text, ep)) => {
                eprintln!(
                    "[stt] ok via {} model={} ({} char(s))",
                    ep.backend_name,
                    ep.model,
                    text.chars().count()
                );
                return Ok(Some(text));
            }
            Err(e) => {
                eprintln!("[stt] HTTP backends failed: {e}; trying CLI fallbacks");
            }
        }
    }

    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || transcribe_cli(&path))
        .await
        .map_err(|e| anyhow::anyhow!("transcribe join: {e}"))?
}

/// Sync CLI-only path (tests / last resort).
pub fn transcribe_cli(path: &Path) -> anyhow::Result<Option<String>> {
    if !path.is_file() {
        anyhow::bail!("audio file not found: {}", path.display());
    }
    if let Ok(tmpl) = std::env::var("PIRS_CLAW_TRANSCRIBE_CMD") {
        let cmd = tmpl.replace("{path}", &path.display().to_string());
        let out = Command::new("sh").arg("-c").arg(&cmd).output()?;
        if out.status.success() {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !t.is_empty() {
                return Ok(Some(t));
            }
        }
        return Ok(None);
    }
    for bin in ["whisper", "whisper-cpp", "faster-whisper"] {
        if which(bin).is_none() {
            continue;
        }
        let out = Command::new(bin)
            .arg(path.as_os_str())
            .arg("--output_format")
            .arg("txt")
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !t.is_empty() {
                    return Ok(Some(t));
                }
            }
        }
    }
    Ok(None)
}

/// Synthesize speech when TTS backends are configured.
pub async fn synthesize_speech(
    text: &str,
    voice: Option<&str>,
    format: Option<&str>,
) -> anyhow::Result<(Vec<u8>, SpeechEndpoint)> {
    let route = resolve_speech_route(SpeechKind::Tts, None);
    let opts = SpeakOptions {
        voice: voice.map(|s| s.to_string()).or_else(|| non_empty("PIRS_TTS_VOICE")),
        response_format: format
            .map(|s| s.to_string())
            .or_else(|| non_empty("PIRS_TTS_FORMAT"))
            .or_else(|| Some("opus".into())),
        ..Default::default()
    };
    speak_with_failover(&route, text, &opts)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on") | Ok("always")
    )
}

/// Always try TTS on gateway replies.
pub fn tts_always() -> bool {
    env_truthy("PIRS_CLAW_TTS") || env_truthy("PIRS_TTS_REPLIES")
}

fn env_falsey(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref(),
        Ok("0") | Ok("false") | Ok("no") | Ok("off")
    )
}

/// TTS when the inbound message was a voice note.
///
/// Default: **on** when any TTS backend is configured (local pirs-audio / OpenAI / espeak
/// via speech route), unless explicitly disabled with `PIRS_CLAW_TTS_ON_VOICE=0`.
pub fn tts_on_voice() -> bool {
    if tts_always() {
        return true;
    }
    if env_falsey("PIRS_CLAW_TTS_ON_VOICE") {
        return false;
    }
    if env_truthy("PIRS_CLAW_TTS_ON_VOICE") {
        return true;
    }
    // Default opt-in when we can actually synthesize.
    tts_backends_configured()
}

pub fn tts_backends_configured() -> bool {
    if !resolve_speech_route(SpeechKind::Tts, None)
        .endpoints
        .is_empty()
    {
        return true;
    }
    // espeak on PATH can be used via pirs-audio TTS cmd or local daemon
    which("espeak-ng").is_some() || which("espeak").is_some()
}

fn non_empty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_errors() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(transcribe_audio(Path::new("/no/such/audio.ogg")))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn cli_no_transcriber_returns_none_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.ogg");
        std::fs::write(&p, b"").unwrap();
        std::env::remove_var("PIRS_CLAW_TRANSCRIBE_CMD");
        let r = transcribe_cli(&p).unwrap();
        assert!(r.is_none() || r.as_ref().map(|s| s.is_empty()).unwrap_or(true));
    }
}
