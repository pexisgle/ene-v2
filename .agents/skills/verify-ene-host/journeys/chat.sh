#!/usr/bin/env bash
# One real conversation turn through the real host and the real provider.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/chat.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/real-turn
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }

rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null && break
  sleep 0.1
done

echo '=== register the provider as a connection ==='
rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}}]}" | head -c 200; echo
echo '=== create the companion on that model ==='
companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "companion: $companion"
echo '=== send one message ==='
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"smoke-1\",\"text\":\"あなたの名前を一文だけで答えてください。\"}" | head -c 200; echo
echo '=== wait for the reply ==='
for _ in $(seq 1 120); do
  history=$(rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":10}")
  reply=$(echo "$history" | python3 -c '
import json,sys
items=json.load(sys.stdin).get("items",[])
said=[i for i in items if i.get("role")!="MESSAGE_ROLE_USER" and i.get("text")]
print(said[-1]["text"] if said else "")
' 2>/dev/null)
  [ -n "$reply" ] && break
  sleep 1
done
echo "reply: ${reply:-<none>}"
echo "$history" >"$run/evidence/history.json"
echo '=== cleanup ==='
kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
[ -n "${reply:-}" ] && [ "$reply" != "<none>" ]
