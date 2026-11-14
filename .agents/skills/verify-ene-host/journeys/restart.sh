#!/usr/bin/env bash
# Acceptance checks for the binding-failure fix, against the real binary.
#   direnv exec . bash .agents/skills/verify-ene-host/journeys/restart.sh
# Exit 0 when every check passes.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
binary=$repo/target/debug/ene-host
connect=$skill/scripts/connect.py
run=$out/fix-check
pass=0
fail=0
pid=

check() { # name, status
  if [ "$2" -eq 0 ]; then
    echo "PASS  $1"
    pass=$((pass + 1))
  else
    echo "FAIL  $1"
    fail=$((fail + 1))
  fi
}

rpc() { # service/method, json
  ENE_HOST_CONNECTION=$run/runtime/connection.json \
    python3 "$connect" rpc "$1" "$2" 2>/dev/null
}

start_host() { # log-suffix
  local deadline
  ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
    "$binary" >/dev/null 2>"$run/host.$1.err" &
  pid=$!
  deadline=$((SECONDS + 20))
  while [ $SECONDS -lt $deadline ]; do
    if [ -s "$run/runtime/connection.json" ] && rpc ene.host.v1.HostService/GetSnapshot '{}' >/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

stop_host() {
  kill -TERM "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  pid=
}

field() { # json, python expression over the parsed object
  python3 -c "import json,sys; print(json.loads(sys.argv[1])$1)" "$2" 2>/dev/null
}

rm -rf "$run"
mkdir -p "$run"

echo '=== CreateCompanion with no assignments'
start_host boot1
check 'the host starts on an empty state dir' $?

cursor=$(rpc ene.host.v1.HostService/GetSnapshot '{}')
epoch=$(field '["cursor"]["epoch"]' "$cursor")
seq=$(field '["cursor"]["seq"]' "$cursor")

create=$(rpc ene.host.v1.HostService/CreateCompanion '{"name":"Mio","mind":"kind"}')
code=$?
echo "  CreateCompanion -> $create"
check 'CreateCompanion is accepted' $code
echo "$create" | grep -q '"id"'
check 'the response carries the stored companion' $?
listed=$(rpc ene.host.v1.HostService/ListCompanions '{}')
echo "$listed" | grep -q '"name":"Mio"'
check 'ListCompanions shows the companion' $?

echo "=== Notices after the cursor (epoch $epoch, seq $seq)"
replay=$(ENE_HOST_CONNECTION=$run/runtime/connection.json \
  python3 "$connect" stream ene.host.v1.HostService/Subscribe \
  "{\"cursor\":{\"epoch\":\"$epoch\",\"seq\":\"$seq\"}}" --count 2 --timeout 3 2>/dev/null)
echo "$replay" | sed 's/^/  /'
echo "$replay" | grep -q 'model_unavailable'
check 'a model_unavailable notice reaches the client' $?

echo '=== Restart over the state the failed binding left behind'
stop_host
start_host boot2
check 'the host reports ready after a restart' $?
ready=$(field '["health"]["ready"]' "$(rpc ene.host.v1.HostService/GetSnapshot '{}')")
[ "$ready" = "True" ]
check 'GetSnapshot reports health.ready' $?

echo '=== Disabled assignment'
rpc ene.host.v1.SettingsService/UpdateSettings '{"expectedRevision":"1","connections":[{"id":"openai","name":"main","enabled":true,"expectedRevision":"0","kind":"CONNECTION_KIND_LLM","config":{"llm":{"endpoint":{"adapter":"openai","endpoint":"https://api.openai.com/v1"},"modelIds":["gpt-4o-mini"]}}}]}' >/dev/null
rpc ene.host.v1.HostService/CreateCompanion '{"name":"Kai","mind":"calm","assignments":{"companion":{"connectionId":"openai","modelId":"gpt-4o-mini"},"task":{"connectionId":"openai","modelId":"gpt-4o-mini"}}}' >/dev/null
disabled=$(rpc ene.host.v1.SettingsService/UpdateSettings '{"expectedRevision":"2","connections":[{"id":"openai","name":"main","enabled":false,"expectedRevision":"1","kind":"CONNECTION_KIND_LLM","config":{"llm":{"endpoint":{"adapter":"openai","endpoint":"https://api.openai.com/v1"},"modelIds":["gpt-4o-mini"]}}}]}')
code=$?
echo "  UpdateSettings -> ${disabled:0:120}"
check 'the assigned connection is disabled in settings' $code
stop_host
start_host boot3
check 'the host reports ready with the connection disabled' $?
# Read all events this boot, then look for the notice that explains the state.
epoch=$(field '["cursor"]["epoch"]' "$(rpc ene.host.v1.HostService/GetSnapshot '{}')")
replay=$(ENE_HOST_CONNECTION=$run/runtime/connection.json \
  python3 "$connect" stream ene.host.v1.HostService/Subscribe \
  "{\"cursor\":{\"epoch\":\"$epoch\",\"seq\":\"0\"}}" --count 20 --timeout 2 2>/dev/null)
echo "$replay" | grep -o 'the assigned connection is disabled' | head -1 | sed 's/^/  notice: /'
echo "$replay" | grep -q 'the assigned connection is disabled'
check 'the notice names the disabled connection' $?

stop_host
echo
echo "=== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
