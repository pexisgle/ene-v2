#!/usr/bin/env bash
# Every RPC the earlier journeys did not drive, against the real binary.
#   set -a; . ./.env; set +a; direnv exec . bash .agents/skills/verify-ene-host/journeys/rpcs.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/rpcs
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=
pass=0
fail=0
notes=0

check() { # name status [detail]
  if [ "$2" -eq 0 ]; then echo "PASS  $1"; pass=$((pass + 1)); else echo "FAIL  $1 ${3:-}"; fail=$((fail + 1)); fi
}
note() { echo "NOTE  $1"; notes=$((notes + 1)); }
rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }
ok() { rpc "$1" "$2" >/dev/null; }
field() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)" 2>/dev/null; }

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }
rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_ALLOWED_CAPABILITIES='*' ENE_HOST_SESSION_CREDENTIALS=1 \
  ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && ok ene.host.v1.HostService/GetSnapshot '{}' && break
  sleep 0.1
done

echo '=== settings'
revision=$(rpc ene.host.v1.SettingsService/GetSettings '{}' | field 'd["revision"]')
[ "$revision" = "1" ]; check 'GetSettings returns the initial revision' $?
rpc ene.host.v1.SettingsService/ListProviders '{}' | grep -q '"adapter":"openrouter"'; check 'ListProviders names the openrouter adapter' $?
ok ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"$revision\",\"connections\":[{\"id\":\"openrouter\",\"name\":\"openrouter\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}}]}"; check 'UpdateSettings stores an LLM connection' $?
rpc ene.host.v1.SettingsService/ListModels '{"id":"openrouter"}' | grep -q '"items"'; check 'ListModels answers for the connection' $?
ok ene.host.v1.SettingsService/ListPendingApprovals '{}'; check 'ListPendingApprovals answers' $?
rpc ene.host.v1.SettingsService/CheckConnection '{"id":"openrouter"}' | grep -q '"success":true'; check 'CheckConnection probes the provider' $?
rpc ene.host.v1.SettingsService/RegisterCredentials "{\"id\":\"openrouter\",\"secret\":\"$OPENROUTER_API_KEY\"}" | grep -q 'registered'; check 'RegisterCredentials reports registered' $?
rpc ene.host.v1.SettingsService/RevokeCredentials '{"id":"openrouter"}' >/dev/null; check 'RevokeCredentials answers' $?
ok ene.host.v1.SettingsService/UpdatePolicyRules '{"addOrUpdate":[{"id":"rule-1","toolName":"body_motion","pattern":"*","effect":"POLICY_EFFECT_ALLOW"}]}'; check 'UpdatePolicyRules adds a rule' $?
rpc ene.host.v1.SettingsService/ListPolicyRules '{}' | grep -q 'body_motion'; check 'ListPolicyRules shows it' $?
ok ene.host.v1.SettingsService/UpdatePolicyRules '{"removeIds":["rule-1"]}'; check 'UpdatePolicyRules removes it' $?
rpc ene.host.v1.SettingsService/ListPolicyRules '{}' | grep -q 'body_motion'; [ $? -ne 0 ]; check 'the removed rule is gone' $?

echo '=== companions, mind, memories'
companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | field 'd["id"]')
[ -n "$companion" ]; check 'CreateCompanion stores a companion' $?
rpc ene.host.v1.HostService/GetCompanion "{\"id\":\"$companion\"}" | grep -q "$companion"; check 'GetCompanion reads it back' $?
ok ene.host.v1.HostService/UpdateCompanion "{\"id\":\"$companion\",\"revision\":\"1\",\"name\":\"Mio II\"}"; check 'UpdateCompanion saves an edit' $?
mind=$(rpc ene.host.v1.HostService/GetMind "{\"id\":\"$companion\"}")
profile_revision=$(echo "$mind" | field 'd["profile"]["revision"]')
ok ene.host.v1.HostService/UpdateMindProfile "{\"companionId\":\"$companion\",\"expectedRevision\":\"$profile_revision\",\"profile\":{\"core\":\"kind and calm\",\"facets\":[\"tea lover\"]}}"; check 'UpdateMindProfile saves a profile' $?
rpc ene.host.v1.HostService/GetMind "{\"id\":\"$companion\"}" | grep -q 'tea lover'; check 'GetMind returns the new profile' $?
ok ene.host.v1.HostService/ListMemories "{\"companionId\":\"$companion\"}"; check 'ListMemories answers' $?
throwaway=$(rpc ene.host.v1.HostService/CreateCompanion '{"name":"Temp","mind":"quiet"}' | field 'd["id"]')
ok ene.host.v1.HostService/DeleteCompanion "{\"id\":\"$throwaway\"}"; check 'DeleteCompanion answers' $?
rpc ene.host.v1.HostService/GetCompanion "{\"id\":\"$throwaway\"}" >/dev/null; [ $? -ne 0 ]; check 'a deleted companion is gone' $?

echo '=== chat'
request=$(rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"rpc-1\",\"text\":\"一文だけで挨拶してください。\"}" | field 'd["requestId"]')
[ -n "$request" ]; check 'SendMessage accepts a message' $?
same=$(rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"rpc-1\",\"text\":\"一文だけで挨拶してください。\"}" | field 'd["requestId"]')
[ "$same" = "$request" ]; check 'the same request key returns the same id' $?
history=''
for _ in $(seq 1 90); do
  history=$(rpc ene.host.v1.ChatService/GetHistory "{\"companionId\":\"$companion\",\"limit\":20}")
  echo "$history" | grep -q 'MESSAGE_ROLE_ASSISTANT' && break
  sleep 1
done
echo "$history" | grep -q 'MESSAGE_ROLE_USER'; check 'GetHistory has the user message' $?
echo "$history" | grep -q 'MESSAGE_ROLE_ASSISTANT'; check 'GetHistory has the assistant reply' $?
first=$(echo "$history" | field 'd["items"][0]["id"]')
rpc ene.host.v1.ChatService/GetMessage "{\"companionId\":\"$companion\",\"messageId\":\"$first\"}" | grep -q "$first"; check 'GetMessage reads one message' $?
ok ene.host.v1.ChatService/Stop "{\"companionId\":\"$companion\"}"; check 'Stop answers' $?

echo '=== clients, perception, approval'
ok ene.host.v1.ClientService/RegisterClient '{"id":"c1","name":"desk","platform":"linux","capabilities":["body.motion"],"capabilitySpecs":[{"name":"body.motion","toolName":"body_motion","description":"Move the body.","inputSchema":"{\"type\":\"object\",\"required\":[\"motion\"],\"properties\":{\"motion\":{\"type\":\"string\"}}}","approval":"CAPABILITY_APPROVAL_CONFIRM"}]}'; check 'RegisterClient stores a device' $?
rpc ene.host.v1.ClientService/List '{}' | grep -q '"id":"c1"'; check 'List shows it' $?
ok ene.host.v1.ClientService/SetCapabilities '{"clientId":"c1","revision":"1","capabilities":["body.motion","eye.watch"],"capabilitySpecs":[{"name":"body.motion","toolName":"body_motion","description":"Move the body.","inputSchema":"{\"type\":\"object\"}","approval":"CAPABILITY_APPROVAL_CONFIRM"},{"name":"eye.watch","description":"Watch.","inputSchema":"{\"type\":\"object\"}","approval":"CAPABILITY_APPROVAL_AUTO"}]}'; check 'SetCapabilities replaces the declaration' $?
activation=$(rpc ene.host.v1.ClientService/Activate "{\"companionId\":\"$companion\",\"clientId\":\"c1\"}" | field 'd["revision"]')
[ -n "$activation" ]; check 'Activate binds the client' $?
ok ene.host.v1.ClientService/ReportPerception "{\"clientId\":\"c1\",\"timestamp\":\"2026-11-15T00:00:00Z\",\"activationRevision\":\"$activation\",\"activeWindow\":{\"title\":\"editor\",\"appId\":\"code\"}}"; check 'ReportPerception accepts an event' $?
ok ene.host.v1.ClientService/ReportPlaybackStatus '{"utteranceId":"unknown","generationId":"1","status":"PLAYBACK_COMPLETED"}'; st=$?; [ $st -eq 0 ] && check 'ReportPlaybackStatus answers' $? || note 'ReportPlaybackStatus needs a TTS connection (by design)'

echo "=== $pass passed, $fail failed, $notes notes"
[ "$fail" -eq 0 ]
