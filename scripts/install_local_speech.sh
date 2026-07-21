#!/usr/bin/env bash
# Install/run lightweight **Rust** pirs-audio daemon for pirs.
#
#   scripts/install_local_speech.sh --dir ~/.pirs/speech --url http://127.0.0.1:8090/v1
set -euo pipefail

DIR="${HOME}/.pirs/speech"
URL="http://127.0.0.1:8090/v1"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dir) DIR="$2"; shift 2 ;;
    --url) URL="$2"; shift 2 ;;
    --no-stt) shift ;; # accepted for compat; Rust daemon has no pip STT
    -h|--help) sed -n '1,12p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

mkdir -p "$DIR"
PORT=8090
if [[ "$URL" =~ :([0-9]+) ]]; then PORT="${BASH_REMATCH[1]}"; fi

# Locate pirs-audio binary: PATH, cargo target, or build from repo
BIN=""
if command -v pirs-audio >/dev/null 2>&1; then
  BIN="$(command -v pirs-audio)"
fi
if [[ -z "$BIN" ]]; then
  for c in \
    "${CARGO_TARGET_DIR:-}/debug/pirs-audio" \
    "${CARGO_TARGET_DIR:-}/release/pirs-audio" \
    /home/driver/hero/build/target/debug/pirs-audio \
    /home/driver/hero/build/target/release/pirs-audio \
    "$HOME/.cargo/bin/pirs-audio"
  do
    if [[ -n "$c" && -x "$c" ]]; then BIN=$c; break; fi
  done
fi

REPO=""
for c in \
  "$(cd "$(dirname "$0")/.." && pwd)" \
  /home/driver/xmoncode/pirs
do
  if [[ -f "$c/crates/pirs-audio/Cargo.toml" ]]; then REPO=$c; break; fi
done

if [[ -z "$BIN" && -n "$REPO" ]]; then
  echo "[install_local_speech] building pirs-audio from $REPO"
  (cd "$REPO" && cargo build -p pirs-audio)
  BIN="$REPO/target/debug/pirs-audio"
  # workspace may use CARGO_TARGET_DIR
  if [[ ! -x "$BIN" && -n "${CARGO_TARGET_DIR:-}" ]]; then
    BIN="${CARGO_TARGET_DIR}/debug/pirs-audio"
  fi
  if [[ ! -x "$BIN" ]]; then
    BIN="/home/driver/hero/build/target/debug/pirs-audio"
  fi
fi

if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "[install_local_speech] pirs-audio binary not found; build with: cargo build -p pirs-audio" >&2
  exit 1
fi

cp -f "$BIN" "$DIR/pirs-audio"
chmod +x "$DIR/pirs-audio"

cat > "$DIR/run.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
export PIRS_AUDIO_HOST="\${PIRS_AUDIO_HOST:-127.0.0.1}"
export PIRS_AUDIO_PORT="\${PIRS_AUDIO_PORT:-$PORT}"
# Lightweight: mock STT/TTS unless whisper CLI / espeak / PIRS_AUDIO_*_CMD set
export PIRS_AUDIO_ALLOW_MOCK="\${PIRS_AUDIO_ALLOW_MOCK:-1}"
exec "$DIR/pirs-audio" serve --host "\$PIRS_AUDIO_HOST" --port "\$PIRS_AUDIO_PORT" "\$@"
EOF
chmod +x "$DIR/run.sh"

mkdir -p "$HOME/.config/systemd/user"
cat > "$HOME/.config/systemd/user/pirs-audio.service" <<EOF
[Unit]
Description=pirs-audio lightweight OpenAI-compatible STT/TTS (Rust)
After=network.target

[Service]
Type=simple
ExecStart=$DIR/run.sh
Restart=on-failure
RestartSec=2
Environment=PIRS_AUDIO_PORT=$PORT
Environment=PIRS_AUDIO_HOST=127.0.0.1
Environment=HOME=$HOME

[Install]
WantedBy=default.target
EOF

cat > "$DIR/README.md" <<EOF
# pirs-audio (Rust, lightweight)

Binary: \`$DIR/pirs-audio\`
URL: \`$URL\`

No embedded ONNX/ML — engines are subprocess/CLI:

- STT: \`PIRS_AUDIO_STT_CMD\`, \`whisper\` CLI, or mock
- TTS: \`espeak-ng\`, \`PIRS_AUDIO_TTS_CMD\`, or mock

\`\`\`bash
$DIR/run.sh
# doctor:
$DIR/pirs-audio doctor
pirs-claw speech setup --local --cloud --force --local-url $URL
\`\`\`
EOF

echo "[install_local_speech] installed $DIR/pirs-audio (port=$PORT)"
echo "[install_local_speech] run: $DIR/run.sh"
exit 0
