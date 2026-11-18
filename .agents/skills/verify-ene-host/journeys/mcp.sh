#!/usr/bin/env bash
# A real MCP server becomes a tool the parent agent calls.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/mcp.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/mcp
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }

rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null && break
  sleep 0.1
done

echo '=== register the model and a stdio MCP server ==='
rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[\
{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}},\
{\"id\":\"echo\",\"name\":\"echo\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_MCP\",\"config\":{\"mcp\":{\"endpoint\":{\"adapter\":\"mcp\"},\"allowedTools\":[\"echo\"],\"command\":\"python3\",\"args\":[\"$here/mcp-server.py\"]}}}]}" | head -c 200; echo

companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "companion: $companion"

echo '=== wait for the MCP server to connect ==='
sleep 3
grep -i "mcp" "$run/host.stderr" | head -5 || true

echo '=== ask the model to call the MCP tool ==='
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"mcp-1\",\"text\":\"必ず mcp_echo_echo ツールを text='hello' で1回だけ呼んでください。ツールが返した文字列をそのまま書いてください。\"}" | head -c 120; echo

reply=''
for _ in $(seq 1 90); do
  reply=$(rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":20}" | python3 -c '
import json,sys
items=json.load(sys.stdin).get("items",[])
said=[i for i in items if i.get("role")=="MESSAGE_ROLE_ASSISTANT" and i.get("text")]
print(said[-1]["text"] if said else "")
' 2>/dev/null)
  [ -n "$reply" ] && break
  sleep 1
done
echo "reply: ${reply:-<none>}"
echo "$reply" | grep -q 'ECHO: hello' && echo 'PASS  the model saw the MCP tool result' || { echo 'FAIL  the MCP tool did not reach the model'; exit 1; }
