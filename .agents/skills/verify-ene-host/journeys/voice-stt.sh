#!/usr/bin/env bash
# Speech in, transcript out: a real recording through the real STT provider.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/voice-stt.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
fixture=$out/stt/fixture.pcm
phrase=${PHRASE:-Hello, this is a microphone test.}

[ -n "${OPENAI_API_KEY:-}" ] || { echo 'OPENAI_API_KEY is not set' >&2; exit 2; }
mkdir -p "$out/stt"

echo '=== synthesise the recording (24 kHz 16-bit mono PCM) ==='
curl -sS --max-time 60 https://api.openai.com/v1/audio/speech \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"tts-1\",\"voice\":\"alloy\",\"input\":\"$phrase\",\"response_format\":\"pcm\"}" \
  -o "$fixture" -w 'http %{http_code} bytes %{size_download}\n'
[ -s "$fixture" ] || { echo 'FAIL  no recording'; exit 1; }

echo '=== stream it through the host ==='
cd "$repo"
ENE_STT_FIXTURE=$fixture direnv exec . cargo test -p ene-host --locked --test voice_stt -- --nocapture 2>&1 |
  grep -E "transcript:|skipping|panicked|test result" | head -6
