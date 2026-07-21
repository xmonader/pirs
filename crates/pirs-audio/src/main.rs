//! pirs-audio — lightweight OpenAI-compatible STT/TTS daemon.
//!
//! No ONNX / ort / embedded models. Engines are **subprocess / CLI** only
//! (whisper, espeak, custom commands) plus a built-in mock for wiring tests.
//!
//!   pirs-audio serve --port 8090
//!   POST /v1/audio/transcriptions
//!   POST /v1/audio/speech

mod engines;
mod ffmpeg;
mod server;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "pirs-audio",
    about = "Lightweight OpenAI-compatible local STT/TTS daemon for pirs (no embedded ML)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Commands>,
    /// Shorthand: `pirs-audio --port 8090` ≈ `pirs-audio serve --port 8090`
    #[arg(long, global = true, default_value = "127.0.0.1", env = "PIRS_AUDIO_HOST")]
    host: String,
    #[arg(long, global = true, default_value_t = 8090, env = "PIRS_AUDIO_PORT")]
    port: u16,
    /// STT engine: auto | mock | whisper-cli | cmd
    #[arg(long, global = true, default_value = "auto", env = "PIRS_AUDIO_STT_ENGINE")]
    stt_engine: String,
    /// TTS engine: auto | mock | espeak | cmd
    #[arg(long, global = true, default_value = "auto", env = "PIRS_AUDIO_TTS_ENGINE")]
    tts_engine: String,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the HTTP daemon (default).
    Serve,
    /// Print which engines would be selected and exit.
    Doctor,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = engines::EngineConfig {
        stt_engine: cli.stt_engine.clone(),
        tts_engine: cli.tts_engine.clone(),
        stt_cmd: std::env::var("PIRS_AUDIO_STT_CMD").ok(),
        tts_cmd: std::env::var("PIRS_AUDIO_TTS_CMD").ok(),
        stt_model_id: std::env::var("PIRS_AUDIO_STT_ID").unwrap_or_else(|_| "parakeet-tdt".into()),
        tts_model_id: std::env::var("PIRS_AUDIO_TTS_ID").unwrap_or_else(|_| "kokoro".into()),
        allow_mock: !matches!(
            std::env::var("PIRS_AUDIO_ALLOW_MOCK").as_deref(),
            Ok("0") | Ok("false") | Ok("no")
        ),
    };

    match cli.cmd.unwrap_or(Commands::Serve) {
        Commands::Doctor => {
            let stt = engines::select_stt(&cfg)?;
            let tts = engines::select_tts(&cfg)?;
            println!("stt_engine={}", stt.name());
            println!("tts_engine={}", tts.name());
            println!("stt_model_id={}", cfg.stt_model_id);
            println!("tts_model_id={}", cfg.tts_model_id);
            println!("ffmpeg={}", if ffmpeg::have_ffmpeg() { "yes" } else { "no" });
            Ok(())
        }
        Commands::Serve => {
            server::serve(&cli.host, cli.port, cfg).await
        }
    }
}
