#!/usr/bin/env bash
# A real client over DeviceStream: register, activate, take an operation the
# model asked for, report the result, and see the turn finish.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/device.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/device-stream
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }

rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>>"$run/evidence/stream.err"; }
stream() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 -u "$connect" stream "$@" 2>>"$run/evidence/stream.err"; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime ENE_HOST_ALLOWED_CAPABILITIES='*' \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null && break
  sleep 0.1
done

rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}}]}" >/dev/null
companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "companion: $companion"

echo '=== register the device and activate it for the companion ==='
rpc ene.host.v1.ClientService/RegisterClient '{"id":"c1","name":"desk","platform":"linux","capabilities":["body.motion"],"capabilitySpecs":[{"name":"body.motion","toolName":"body_motion","description":"Move the character body.","inputSchema":"{\"type\":\"object\",\"required\":[\"motion\"],\"properties\":{\"motion\":{\"type\":\"string\"}}}","approval":"CAPABILITY_APPROVAL_AUTO"}]}' | head -c 220; echo
rpc ene.host.v1.ClientService/Activate "{\"companionId\":\"$companion\",\"clientId\":\"c1\"}" | head -c 160; echo

echo '=== open DeviceStream and ask the model to move the body ==='
# The stream stays open through the result: closing it first makes the host
# treat the operation as unanswered.
stream ene.host.v1.ClientService/DeviceStream '{"id":"c1"}' --count 100 --timeout 120 >"$run/evidence/frames.jsonl" &
reader=$!
trap 'kill -TERM "$pid" "$reader" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
sleep 1
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"device-1\",\"text\":\"必ず body_motion ツールを motion='nod' で1回だけ呼んでください。呼んだら「動きました」とだけ返してください。\"}" | head -c 120; echo
for _ in $(seq 1 90); do
  grep -q '"operation"' "$run/evidence/frames.jsonl" && break
  sleep 1
done
cat "$run/evidence/frames.jsonl"

operation=$(python3 - "$run/evidence/frames.jsonl" <<'PY'
import json,sys
op=None
for line in open(sys.argv[1]):
    line=line.strip()
    if not line: continue
    event=json.loads(line)
    if "operation" in event: op=event["operation"]
if op is None: sys.exit(1)
print(op["operationId"], op.get("activationRevision","0"), op.get("input","{}").replace(" ",""))
PY
)
read -r op_id revision op_input <<<"$operation"
echo "operation: $op_id revision=$revision input=$op_input"
[ -n "${op_id:-}" ] || { echo 'FAIL  the model did not reach the device'; exit 1; }

echo '=== report the result from the client ==='
rpc ene.host.v1.ClientService/CompleteOperation "{\"clientId\":\"c1\",\"operationId\":\"$op_id\",\"success\":true,\"output\":\"{}\",\"activationRevision\":\"$revision\"}" | head -c 200; echo

echo '=== wait for the turn to finish ==='
reply=''
for _ in $(seq 1 60); do
  reply=$(rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":10}" | python3 -c '
import json,sys
items=json.load(sys.stdin).get("items",[])
said=[i for i in items if i.get("role")=="MESSAGE_ROLE_ASSISTANT" and i.get("text")]
print(said[-1]["text"] if said else "")
' 2>/dev/null)
  [ -n "$reply" ] && break
  sleep 1
done
echo "reply: ${reply:-<none>}"
state=$(rpc ene.host.v1.ClientService/GetOperation "{\"id\":\"$op_id\"}")
echo "operation state: $state"
kill -TERM "$reader" 2>/dev/null
echo "$state" | grep -q 'DEVICE_OPERATION_STATUS_COMPLETED' && echo 'PASS  the operation completed' || { echo 'FAIL  the operation did not complete'; exit 1; }
[ -n "${reply:-}" ] && echo 'PASS  the turn finished after the device replied' || { echo 'FAIL  the turn did not finish'; exit 1; }
