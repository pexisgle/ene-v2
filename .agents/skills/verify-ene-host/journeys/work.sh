#!/usr/bin/env bash
# Work runs, inquiries, approvals and memory, against the real binary and a
# real provider.  set -a; . ./.env; set +a;
#   direnv exec . bash .agents/skills/verify-ene-host/journeys/work.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/work
model=${MODEL:-openai/gpt-4o-mini}
connect=$skill/scripts/connect.py
pid=
reader=
pass=0
fail=0

check() { if [ "$2" -eq 0 ]; then echo "PASS  $1"; pass=$((pass + 1)); else echo "FAIL  $1 ${3:-}"; fail=$((fail + 1)); fi; }
rpc() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }
stream() { ENE_HOST_CONNECTION=$run/runtime/connection.json python3 -u "$connect" stream "$@" 2>>"$run/evidence/stream.err"; }
ok() { rpc "$1" "$2" >/dev/null; }
field() { python3 -c "import json,sys; d=json.load(sys.stdin); print($1)" 2>/dev/null; }

[ -n "${OPENROUTER_API_KEY:-}" ] || { echo 'OPENROUTER_API_KEY is not set' >&2; exit 2; }
[ -n "${OPENAI_API_KEY:-}" ] || { echo 'OPENAI_API_KEY is not set' >&2; exit 2; }

rm -rf "$run"; mkdir -p "$run/evidence"
ENE_HOST_ALLOWED_CAPABILITIES='*' ENE_HOST_SESSION_CREDENTIALS=1 \
  ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" "$reader" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/runtime/connection.json" ] && ok ene.host.v1.HostService/GetSnapshot '{}' && break
  sleep 0.1
done

rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[\
{\"id\":\"openrouter\",\"name\":\"llm\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_LLM\",\"config\":{\"llm\":{\"endpoint\":{\"adapter\":\"openrouter\",\"endpoint\":\"https://openrouter.ai/api/v1\"},\"modelIds\":[\"$model\"]}}},\
{\"id\":\"openai-tts\",\"name\":\"tts\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_TTS\",\"config\":{\"tts\":{\"endpoint\":{\"adapter\":\"openai\",\"endpoint\":\"https://api.openai.com/v1\"},\"voiceIds\":[\"alloy\"]}}}]}" >/dev/null
companion=$(rpc ene.host.v1.HostService/CreateCompanion "{\"name\":\"Mio\",\"mind\":\"kind\",\"assignments\":{\"companion\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"},\"task\":{\"connectionId\":\"openrouter\",\"modelId\":\"$model\"}}}" | field 'd["id"]')
echo "companion: $companion"

echo '=== memory through the model'
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"mem-1\",\"text\":\"mind_update ツールを operation_id='mem-1', memories=[{\\\"content\\\":\\\"私は緑茶が好き\\\",\\\"importance\\\":7}] で1回だけ呼んでください。呼んだら「保存しました」とだけ返してください。\"}" >/dev/null
memory=''
for _ in $(seq 1 90); do
  memory=$(rpc ene.host.v1.HostService/ListMemories "{\"companionId\":\"$companion\"}" | field 'd["items"][0]["id"] if d.get("items") else ""')
  [ -n "$memory" ] && break
  sleep 1
done
[ -n "$memory" ]; check 'the model stored a memory through mind_update' $?
rpc ene.host.v1.HostService/ControlMemory "{\"companionId\":\"$companion\",\"memoryId\":\"$memory\",\"action\":\"MEMORY_ACTION_FORGET\"}" | grep -q "$memory"; check 'ControlMemory forgets it' $?

echo '=== a work run that asks a question'
work=$(rpc ene.host.v1.WorkService/StartWork "{\"companionId\":\"$companion\",\"title\":\"inquiry\",\"instruction\":\"最初に report ツールで kind='question', content='好きな色は？' を送ってください。回答が届いたら kind='completed', content='回答は<色>です' で完了を報告してください。\",\"clientId\":\"\"}" | field 'd["id"]')
[ -n "$work" ]; check 'StartWork creates a session' $?
request=''
for _ in $(seq 1 90); do
  request=$(rpc ene.host.v1.WorkService/GetWork "{\"workId\":\"$work\"}" | field 'd["pendingInquiries"][0]["requestId"] if d.get("pendingInquiries") else ""')
  [ -n "$request" ] && break
  sleep 1
done
[ -n "$request" ]; check 'the hand asks a question' $?
ok ene.host.v1.WorkService/AnswerInquiry "{\"workId\":\"$work\",\"requestId\":\"$request\",\"answer\":\"青です\",\"clientId\":\"\"}"; check 'AnswerInquiry answers it' $?
state=''
for _ in $(seq 1 90); do
  state=$(rpc ene.host.v1.WorkService/GetWork "{\"workId\":\"$work\"}" | field '(d.get("runs") or [{}])[-1].get("state","")')
  [ "$state" = "RUN_STATUS_COMPLETED" ] && break
  sleep 1
done
[ "$state" = "RUN_STATUS_COMPLETED" ]; check 'the run completes after the answer' $? "($state)"
rpc ene.host.v1.WorkService/ListWorks "{\"companionId\":\"$companion\"}" | grep -q "$work"; check 'ListWorks shows the session' $?
sent=$(rpc ene.host.v1.WorkService/SendWork "{\"workId\":\"$work\",\"requestKey\":\"send-1\",\"text\":\"追加の指示です。report で kind='completed', content='追加も完了' を送ってください。\",\"clientId\":\"\"}")
follow=$(echo "$sent" | field 'd["requestId"]')
[ -n "$follow" ] || echo "  SendWork -> $sent"
[ -n "$follow" ]; check 'SendWork starts a follow-up run' $?

echo '=== stopping a second run'
other=$(rpc ene.host.v1.WorkService/StartWork "{\"companionId\":\"$companion\",\"title\":\"long\",\"instruction\":\"report で kind='activity', content='作業中' を3回送ってから、kind='completed', content='終わりました' を送ってください。\",\"clientId\":\"\"}" | field 'd["id"]')
[ -n "$other" ]; check 'a second session starts' $?
ok ene.host.v1.WorkService/StopWork "{\"workId\":\"$other\"}"; check 'StopWork answers' $?
ok ene.host.v1.WorkService/CloseWork "{\"workId\":\"$other\"}"; check 'CloseWork answers' $?

echo '=== a confirm-gated device operation'
rpc ene.host.v1.ClientService/RegisterClient '{"id":"c1","name":"desk","platform":"linux","capabilities":["body.motion"],"capabilitySpecs":[{"name":"body.motion","toolName":"body_motion","description":"Move the body.","inputSchema":"{\"type\":\"object\",\"required\":[\"motion\"],\"properties\":{\"motion\":{\"type\":\"string\"}}}","approval":"CAPABILITY_APPROVAL_CONFIRM"}]}' >/dev/null
activation=$(rpc ene.host.v1.ClientService/Activate "{\"companionId\":\"$companion\",\"clientId\":\"c1\"}" | field 'd["revision"]')
[ -n "$activation" ]; check 'the device becomes active' $?
stream ene.host.v1.ClientService/DeviceStream '{"id":"c1"}' --count 100 --timeout 150 >"$run/evidence/frames.jsonl" &
reader=$!
sleep 1
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"dev-1\",\"text\":\"必ず body_motion ツールを motion='nod' で1回だけ呼んでください。\"}" >/dev/null
approval=''
for _ in $(seq 1 90); do
  approval=$(rpc ene.host.v1.SettingsService/ListPendingApprovals '{}' | field 'd["items"][0]["requestId"] if d.get("items") else ""')
  [ -n "$approval" ] && break
  sleep 1
done
[ -n "$approval" ]; check 'a confirm-gated call creates an approval' $?
ok ene.host.v1.SettingsService/SubmitApproval "{\"requestId\":\"$approval\",\"decision\":\"APPROVAL_DECISION_APPROVE\"}"; check 'SubmitApproval approves it' $?
operation=''
for _ in $(seq 1 60); do
  operation=$(grep -o '"operationId":"[^"]*"' "$run/evidence/frames.jsonl" 2>/dev/null | head -1 | cut -d'"' -f4)
  [ -n "$operation" ] && break
  sleep 1
done
[ -n "$operation" ]; check 'the device receives the operation' $?
revision=$(grep -o '"activationRevision":"[^"]*"' "$run/evidence/frames.jsonl" 2>/dev/null | head -1 | cut -d'"' -f4)
ok ene.host.v1.ClientService/CompleteOperation "{\"clientId\":\"c1\",\"operationId\":\"$operation\",\"success\":true,\"output\":\"{}\",\"activationRevision\":\"$revision\"}"; check 'CompleteOperation reports the result' $?
rpc ene.host.v1.ClientService/GetOperation "{\"id\":\"$operation\"}" | grep -q 'DEVICE_OPERATION_STATUS_COMPLETED'; check 'the operation is completed' $?
kill -TERM "$reader" 2>/dev/null

echo '=== playback status for a real utterance'
stream ene.host.v1.VoiceService/SubscribeAudioOutput "{\"clientId\":\"c1\",\"activationRevision\":\"$activation\"}" --count 60 --timeout 120 >"$run/evidence/audio.jsonl" &
speaker=$!
trap 'kill -TERM "$pid" "$reader" "$speaker" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
sleep 1
rpc ene.host.v1.ChatService/SendMessage "{\"companionId\":\"$companion\",\"requestKey\":\"tts-1\",\"text\":\"短く一文だけで挨拶してください。\"}" >/dev/null
utterance=''
generation=''
for _ in $(seq 1 60); do
  utterance=$(grep -o '"utteranceId":"[^"]*"' "$run/evidence/audio.jsonl" 2>/dev/null | head -1 | cut -d'"' -f4)
  generation=$(grep -o '"generationId":"[^"]*"' "$run/evidence/audio.jsonl" 2>/dev/null | head -1 | cut -d'"' -f4)
  [ -n "$utterance" ] && break
  sleep 1
done
kill -TERM "$speaker" 2>/dev/null
[ -n "$utterance" ]; check 'the reply is synthesised to an utterance' $?
rpc ene.host.v1.VoiceService/ReportPlaybackStatus "{\"clientId\":\"c1\",\"activationRevision\":\"$activation\",\"utteranceId\":\"$utterance\",\"generationId\":\"$generation\",\"status\":\"PLAYBACK_COMPLETED\",\"completedPcmBytes\":\"1024\"}" | grep -q '"acknowledged":true'; check 'ReportPlaybackStatus acknowledges a real utterance' $?

echo "=== $pass passed, $fail failed"
[ "$fail" -eq 0 ]
