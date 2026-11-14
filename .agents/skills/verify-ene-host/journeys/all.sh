#!/usr/bin/env bash
# Every acceptance journey against the real binary, one after another.
#   set -a; . ./.env; set +a
#   direnv exec . bash .agents/skills/verify-ene-host/journeys/all.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
mkdir -p "$out"
pass=0
fail=0
failed=''

run() {
  name=$1
  if bash "$here/$name.sh" >"$out/$name.log" 2>&1; then
    echo "PASS  $name"
    pass=$((pass + 1))
  else
    echo "FAIL  $name  ($out/$name.log)"
    fail=$((fail + 1))
    failed="$failed $name"
  fi
}

run rpcs
run work
run chat
run schedule
run device
run mcp
run voice-tts
run voice-stt
run acp-sandbox
run restart

echo
echo "=== $pass journeys passed, $fail failed${failed:+:$failed}"
[ "$fail" -eq 0 ]
