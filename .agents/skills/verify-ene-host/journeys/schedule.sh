#!/usr/bin/env bash
# A due schedule must not run a turn when the companion's model cannot be bound.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/schedule.sh
# Exit 0 when the schedule fires, records its firing, and starts no turn.
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/schedule-guard
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }

rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }
db() { python3 -c "
import sqlite3,sys
db=sqlite3.connect('$run/state/host.sqlite3')
print(db.execute(sys.argv[1]).fetchall())
" "$1"; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null && break
  sleep 0.1
done

enabled="{\"expectedRevision\":\"1\",\"connections\":[{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}}]}"
disabled="{\"expectedRevision\":\"2\",\"connections\":[{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":false,\"expectedRevision\":\"1\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}}]}"
rpc ene.host.v1.SettingsService/UpdateSettings "$enabled" >/dev/null
companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
count() {
  rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":50}" |
    python3 -c 'import json,sys; print(len(json.load(sys.stdin).get("items",[])))'
}
at=$(date -u -d '+25 seconds' +%Y-%m-%dT%H:%M:%SZ)
echo "companion: $companion  schedule at: $at"

echo '=== turn 1: ask the model to register a one-shot schedule ==='
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"schedule-1\",\"text\":\"schedule_create ツールを1回だけ呼んでください。引数は title='water', instruction='REMINDER-MARKER', run_at='$at' です。呼んだら「登録しました」とだけ返してください。\"}" >/dev/null
for _ in $(seq 1 120); do
  rows=$(db "select count(*) from schedules")
  [ "$rows" != "[(0,)]" ] && break
  sleep 1
done
echo "schedules: $(db 'select id, title, next_run_at, enabled from schedules')"
if [ "$rows" = "[(0,)]" ]; then
  echo 'FAIL  the model did not register a schedule'
  rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":10}" | head -c 600; echo
  exit 1
fi

echo '=== wait for the first turn to settle ==='
for _ in $(seq 1 90); do
  [ "$(count)" -ge 2 ] && break
  sleep 1
done

echo '=== disable the model before the schedule is due ==='
rpc ene.host.v1.SettingsService/UpdateSettings "$disabled" >/dev/null
epoch=$(rpc ene.host.v1.HostService/GetSnapshot '{}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["cursor"]["epoch"])')
before=$(count)
echo "history items before the due time: $before"
rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":50}" >"$run/evidence/before.json"
sleep 30

echo '=== after the due time ==='
echo "schedules: $(db 'select id, title, next_run_at, enabled from schedules')"
echo "history: $(rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":20}")" >"$run/evidence/history.json"
replay=$(ENE_HOST_CONNECTION=$run/runtime/connection.json \
  python3 "$connect" stream ene.host.v1.HostService/Subscribe \
  "{\"cursor\":{\"epoch\":\"$epoch\",\"seq\":\"0\"}}" --count 40 --timeout 2 2>/dev/null)
echo "$replay" | grep -c 'model_unavailable' | sed 's/^/  model_unavailable notices: /'

after=$(count)
echo "history items after the due time: $after"
fired=$(db "select count(*) from schedules where next_run_at is null and enabled = 0")
[ "$fired" = "[(1,)]" ] && echo 'PASS  the schedule recorded its firing' || echo 'FAIL  the schedule did not fire'
[ "$after" = "$before" ] && echo 'PASS  no scheduled turn ran' || { echo 'FAIL  a turn ran after the model was disabled'; exit 1; }
echo "$replay" | grep -q 'model_unavailable' && echo 'PASS  the client was told the model is unavailable' || echo 'FAIL  no notice'
[ "$fired" = "[(1,)]" ] && [ "$after" = "$before" ] && echo "$replay" | grep -q 'model_unavailable'
