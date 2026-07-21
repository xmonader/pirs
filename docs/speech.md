# Speech (STT / TTS) — multi-backend

pirs talks to **OpenAI-compatible** speech daemons the same way it talks to
chat backends: `[[backends]]` + model aliases with a **serve failover chain**.

Heavy models (sherpa-onnx **Parakeet**, **Kokoro**, Whisper, …) stay **out of
process**. pirs is a thin HTTP client.

## Endpoints expected

| API | Method | Notes |
|-----|--------|--------|
| `{base}/audio/transcriptions` | `POST` multipart | fields: `file`, `model`; optional `language` |
| `{base}/audio/speech` | `POST` JSON | `{ model, input, voice?, response_format? }` → audio bytes |

`base` is typically `http://127.0.0.1:8090/v1` or `https://api.openai.com/v1`.

## Config (`~/.pirs/config.toml`)

```toml
[[backends]]
name = "speech-local"
kind = "openai_compatible"
base_url = "http://127.0.0.1:8090/v1"

[[backends]]
name = "openai-speech"
kind = "openai_compatible"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[[models]]
alias = "stt-default"
caps = ["stt"]
serve = [
  { backend = "speech-local", model = "parakeet-tdt" },
  { backend = "openai-speech", model = "whisper-1" },
]

[[models]]
alias = "tts-default"
caps = ["tts"]
serve = [
  { backend = "speech-local", model = "kokoro" },
  { backend = "openai-speech", model = "tts-1" },
]
```

Ordered `serve` list = **automatic failover** (local down → cloud).

## Env (no registry)

| Variable | Purpose |
|----------|---------|
| `PIRS_SPEECH_BASE_URL` | OpenAI-compatible root (`…/v1`) |
| `PIRS_SPEECH_API_KEY` | Optional bearer |
| `GROQ_API_KEY` | Auto STT via Groq Whisper (`whisper-large-v3-turbo`) |
| `OPENAI_API_KEY` | Auto STT (`whisper-1`) + TTS (`tts-1`) |
| `PIRS_STT_MODEL` / `PIRS_TTS_MODEL` | Model ids / aliases |
| `PIRS_TTS_VOICE` | TTS voice name |
| `PIRS_TTS_FORMAT` | e.g. `opus`, `mp3` |
| `PIRS_CLAW_TRANSCRIBE_CMD` | Shell fallback with `{path}` |
| `PIRS_CLAW_TTS=1` | Gateway: voice reply every time |
| `PIRS_CLAW_TTS_ON_VOICE=1` | Gateway: voice reply when user sent VN |

CLI fallbacks: `whisper`, `whisper-cpp`, `faster-whisper` on `PATH`.

## Setup helper

```bash
# Enable cloud Whisper failover from keys in ~/.pirs/secrets.env
# (GROQ_API_KEY and/or OPENAI_API_KEY)
pirs-claw speech setup --cloud --force
pirs-claw speech status

# Install + start **pirs-audio** (local OpenAI-compatible daemon) under ~/.pirs/speech
pirs-claw speech setup --local --force --local-url http://127.0.0.1:8090/v1
# both local first + cloud failover:
pirs-claw speech setup --cloud --local --force
```

### pirs-audio daemon (Rust, lightweight)

Workspace crate: **`crates/pirs-audio`**. No ONNX/ort/embedded models — thin HTTP
front door that shells out to CLIs (or mock).

```bash
cargo build -p pirs-audio
pirs-audio doctor
pirs-audio serve --port 8090
# or: pirs-claw speech setup --local --force --local-url http://127.0.0.1:8090/v1
```

| Engine | STT | TTS |
|--------|-----|-----|
| `PIRS_AUDIO_STT_CMD` / `PIRS_AUDIO_TTS_CMD` | custom | custom |
| `whisper` CLI | yes | — |
| `espeak-ng` | — | yes |
| mock | yes | yes |

Advertised model ids: `parakeet-tdt` / `kokoro` (stable registry aliases).

`setup --cloud` writes a managed block into `~/.pirs/config.toml` with an ordered
serve chain (local if requested → Groq → OpenAI). Env keys alone also work
without rewriting config (Groq/OpenAI are discovered automatically).

## CLI

```bash
pirs-claw speech status
pirs-claw speech setup --cloud
pirs-claw transcribe /path/to/note.ogg
```

## Telegram gateway

Voice notes are downloaded, transcribed via the STT chain, and stored as:

```text
[transcribed voice] …
```

Optional TTS: set `PIRS_CLAW_TTS_ON_VOICE=1` (or `PIRS_CLAW_TTS=1`) with a TTS
backend; replies also go out as `sendVoice` when synthesis succeeds.

## Daemon sizes (approx.)

| Model | Disk |
|-------|------|
| Parakeet 0.6B int8 | ~650 MB |
| Kokoro-82M int8 | ~90 MB (+ ~27 MB voices) |
| Cloud Whisper/TTS | 0 local |

## Smoke without a real model

Point `PIRS_SPEECH_BASE_URL` at any OpenAI-compatible audio server (or a tiny
mock that returns fixed JSON/bytes). Failover still walks the serve list.
