#!/usr/bin/env bash
# Lifecycle for one isolated ene-host verification instance.
#
#   host.sh start [RUN_ID]    build, start the host, wait until ready
#   host.sh doctor [RUN_ID]   read-only check: is this instance worth driving?
#   host.sh restart [RUN_ID]  SIGTERM and start again on the same state (persistence)
#   host.sh stop [RUN_ID]     SIGTERM the process we started, remove scratch state
#   host.sh path [RUN_ID]     print the run directory
#
# `stop` deletes state/ and runtime/ (scratch); `restart` keeps them. Use restart
# to prove that something survived a host restart, stop to clean up.
#
# A run directory is target/verify-ene-host/runs/<RUN_ID> (override the root with
# VERIFY_ROOT, the whole directory with VERIFY_RUN_DIR). Calls during the run read
# $RUN_DIR/connection.json through connect.py.
#
# Isolation: every run gets its own state and runtime directory, and the host
# binds 127.0.0.1:0, so runs never share a port or a lock. Never point this at a
# real user's state directory.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../../.." && pwd)
root=${VERIFY_ROOT:-$repo/target/verify-ene-host/runs}
run_id=${2:-$(date +%Y%m%d-%H%M%S)}
run=${VERIFY_RUN_DIR:-$root/$run_id}
connection=$run/runtime/connection.json
pid_file=$run/host.pid
binary=$repo/target/debug/ene-host

fail() { echo "host.sh: $*" >&2; exit 1; }

# Always rebuild (incremental) so a stale binary can never stand in for the
# change under test. VERIFY_SKIP_BUILD=1 opts out for a fast drive loop.
build() {
  [ "${VERIFY_SKIP_BUILD:-0}" = 1 ] && [ -x "$binary" ] && return 0
  echo "host.sh: building ene-host" >&2
  if command -v cargo >/dev/null 2>&1; then
    (cd "$repo" && cargo build --locked -p ene-host) || fail "cargo build failed"
  else
    (cd "$repo" && direnv exec . cargo build --locked -p ene-host) || fail "cargo build failed"
  fi
}

wait_ready() {
  local deadline=$((SECONDS + 30))
  while [ $SECONDS -lt $deadline ]; do
    if [ -s "$connection" ] \
      && ENE_HOST_CONNECTION=$connection python3 "$here/connect.py" rpc ene.host.v1.HostService/GetSnapshot >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

launch() {
  # The host creates state/ and runtime/ itself and insists on 0700; do not pre-create them.
  ENE_HOST_STATE_DIR=$run/state ENE_HOST_RUNTIME_DIR=$run/runtime \
    "$binary" >/dev/null 2>"$run/host.stderr" &
  echo $! >"$pid_file"
  if ! wait_ready; then
    tail -n 20 "$run/host.stderr" >&2
    kill -9 "$(cat "$pid_file")" 2>/dev/null
    rm -f "$pid_file"
    fail "host did not become ready within 30s"
  fi
  cp "$connection" "$run/evidence/connection.json"
  chmod 600 "$run/evidence/connection.json"
  echo "$run"
}

start() {
  [ -e "$run" ] && [ -s "$pid_file" ] && fail "run directory $run already has a host; stop it first"
  mkdir -p "$run/evidence"
  build
  launch
}

# SIGTERM the recorded host and wait. Keeps state/, unlike stop.
terminate() {
  [ -s "$pid_file" ] || fail "no host pid in $run; start one first"
  local pid
  pid=$(cat "$pid_file")
  kill -TERM "$pid" 2>/dev/null
  for _ in $(seq 1 300); do kill -0 "$pid" 2>/dev/null || break; sleep 0.05; done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null
    echo "host.sh: host $pid ignored SIGTERM and was killed" >&2
  fi
  rm -f "$pid_file"
}

# Restart on the same state: the persistence check that stop cannot provide.
restart() {
  [ -e "$run/state" ] || fail "no state in $run; start one first (stop deletes state)"
  build
  terminate
  launch
}

doctor() {
  local rc=0
  status() { # status <ok|bad> <check> <detail>
    if [ "$1" = ok ]; then printf 'PASS  %s\n' "$2"; else printf 'FAIL  %s (%s)\n' "$2" "$3"; rc=1; fi
  }
  [ -s "$pid_file" ] && kill -0 "$(cat "$pid_file")" 2>/dev/null \
    && status ok "host process is alive (pid $(cat "$pid_file"))" \
    || status bad "host process is alive" "no live pid in $pid_file"
  [ -s "$connection" ] \
    && status ok "connection.json exists" \
    || status bad "connection.json exists" "$connection missing"
  [ "$(stat -c %a "$run/runtime" 2>/dev/null)" = 700 ] \
    && status ok "runtime dir is 0700" \
    || status bad "runtime dir is 0700" "$(stat -c %a "$run/runtime" 2>/dev/null)"
  [ "$(stat -c %a "$connection" 2>/dev/null)" = 600 ] \
    && status ok "connection.json is 0600" \
    || status bad "connection.json is 0600" "$(stat -c %a "$connection" 2>/dev/null)"
  if [ -s "$connection" ]; then
    read -r code ready < <(ENE_HOST_CONNECTION=$connection python3 "$here/connect.py" \
      rpc ene.host.v1.HostService/GetSnapshot 2>/dev/null \
      | python3 -c 'import json,sys; s=json.load(sys.stdin); print(s.get("serverVersion","?"), s.get("health",{}).get("ready",False))' 2>/dev/null)
    if [ "${code:-?}" != "?" ] && [ "${ready:-False}" = "True" ]; then
      status ok "authenticated GetSnapshot reports ready (serverVersion $code)" "ready=$ready"
    else
      status bad "authenticated GetSnapshot reports ready" "serverVersion=${code:-?} ready=${ready:-False}"
    fi
  fi
  echo "doctor: $run"
  return $rc
}

stop() {
  [ -s "$pid_file" ] && terminate
  # Scratch state only; evidence/ and the host log stay for the proof.
  rm -rf "$run/state" "$run/runtime"
  echo "stopped; evidence kept in $run/evidence"
}

case ${1:-} in
  start) start ;;
  restart) restart ;;
  doctor) doctor ;;
  stop) stop ;;
  path) echo "$run" ;;
  *)
    echo "usage: $0 {start|doctor|stop|path} [RUN_ID]" >&2
    exit 2
    ;;
esac
