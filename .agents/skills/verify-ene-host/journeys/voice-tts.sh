#!/usr/bin/env bash
# A real TTS connection turns the assistant's reply into audio chunks on the
# active client's stream.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/voice-tts.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/tts
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }
[ -n "${OPENAI_API_KEY:-}" ] || { echo 'OPENAI_API_KEY is not set' >&2; exit 2; }

rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }
stream() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 -u "$connect" stream "$@" 2>>"$run/evidence/stream.err"; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null && break
  sleep 0.1
done

echo '=== register the model and a TTS connection ==='
rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[\
{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}},\
{\"id\":\"openai-tts\",\"name\":\"openai-tts\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_TTS\",\"config\":{\"tts\":{\"endpoint\":{\"adapter\":\"openai\",\"endpoint\":\"https://api.openai.com/v1\"},\"voiceIds\":[\"alloy\"]}}}]}" | head -c 160; echo

companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "companion: $companion"
rpc ene.host.v1.ClientService/RegisterClient '{"id":"c1","name":"desk","platform":"linux","capabilities":[]}' | head -c 120; echo
rpc ene.host.v1.ClientService/Activate "{\"companionId\":\"$companion\",\"clientId\":\"c1\"}" | head -c 120; echo

echo '=== subscribe to audio and send one message ==='
stream ene.host.v1.VoiceService/SubscribeAudioOutput '{"clientId":"c1","activationRevision":"1"}' \
  --count 60 --timeout 90 >"$run/evidence/audio.jsonl" &
reader=$!
trap 'kill -TERM "$pid" "$reader" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
sleep 1
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"tts-1\",\"text\":\"短く一文だけで挨拶してください。\"}" | head -c 120; echo

for _ in $(seq 1 90); do
  grep -q '"chunk"' "$run/evidence/audio.jsonl" && break
  sleep 1
done
kill -TERM "$reader" 2>/dev/null

chunks=$(grep -c '"chunk"' "$run/evidence/audio.jsonl" 2>/dev/null || echo 0)
finished=$(grep -c '"finished"' "$run/evidence/audio.jsonl" 2>/dev/null || echo 0)
echo "audio chunks: $chunks  finished: $finished"
head -c 300 "$run/evidence/audio.jsonl" 2>/dev/null; echo
[ "$chunks" -gt 0 ] && echo 'PASS  the reply was synthesised' || { echo 'FAIL  no audio chunk arrived'; tail -3 "$run/host.stderr"; exit 1; }
[ "$finished" -gt 0 ] && echo 'PASS  the utterance finished' || echo 'NOTE  no finished event within the window'
