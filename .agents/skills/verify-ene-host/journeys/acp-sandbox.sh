#!/usr/bin/env bash
# A network-isolated ACP child: no route except the host's proxy, and the
# agent reaches its model through it.
#   direnv exec . bash .agents/skills/verify-ene-host/journeys/acp-sandbox.sh
set -uo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
skill=$(cd "$here/.." && pwd)
repo=$(cd "$here/../../../.." && pwd)
out=$repo/target/verify-ene-host
run=$out/acp-sandbox
connect=$skill/scripts/connect.py
pid=

export XDG_CACHE_HOME=${XDG_CACHE_HOME:-/tmp/nix-cache}
bwrap=$(nix build --no-link --print-out-paths nixpkgs#bubblewrap 2>/dev/null)
[ -n "$bwrap" ] || { echo 'FAIL  bubblewrap is not available'; exit 1; }
echo "bwrap: $bwrap"

rm -rf "$run"; mkdir -p "$run/cwd"
cat >"$run/cwd/report.sh" <<EOF
#!/bin/sh
{
  echo "proxy=\${HTTPS_PROXY:-none}"
  echo "http_proxy=\${HTTP_PROXY:-none}"
  echo "direct=\$($(command -v curl) -s -m 5 --noproxy '*' -o /dev/null -w '%{http_code}' https://example.com 2>&1)"
  echo "via_proxy=\$($(command -v curl) -sS -m 25 -o /dev/null -w '%{http_code}' https://example.com 2>&1)"
  echo "plain_http=\$($(command -v curl) -sS -m 5 --proxy http://127.0.0.1:3128 -o /dev/null -w '%{http_code}' http://example.com 2>&1)"
  echo done
} > report.txt 2>&1
EOF
chmod +x "$run/cwd/report.sh"

cd "$run/cwd"
PATH=$bwrap/bin:$PATH ENE_HOST_STATE_DIR=$run/cwd/state ENE_HOST_RUNTIME_DIR=$run/cwd/runtime \
  "$repo/target/debug/ene-host" >/dev/null 2>"$run/cwd/host.stderr" &
pid=$!
trap 'kill -TERM "$pid" 2>/dev/null; wait "$pid" 2>/dev/null' EXIT
for _ in $(seq 1 200); do
  [ -s "$run/cwd/runtime/connection.json" ] && break
  sleep 0.1
done

rpc() { ENE_HOST_CONNECTION=$run/cwd/runtime/connection.json python3 "$connect" rpc "$1" "$2" 2>/dev/null; }

rpc ene.host.v1.SettingsService/UpdateSettings "{\"expectedRevision\":\"1\",\"connections\":[{\"id\":\"acp\",\"name\":\"acp\",\"enabled\":true,\"expectedRevision\":\"0\",\"kind\":\"CONNECTION_KIND_ACP\",\"config\":{\"acp\":{\"command\":\"$run/cwd/report.sh\",\"args\":[],\"endpoint\":{\"adapter\":\"acp\",\"options\":\"{\\\"network\\\":\\\"isolated\\\"}\"}}}}]}" >/dev/null
companion=$(rpc ene.host.v1.HostService/CreateCompanion '{"name":"Mio","mind":"kind","assignments":{"companion":{"connectionId":"acp","modelId":"acp"},"task":{"connectionId":"acp","modelId":"acp"}}}' | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "companion: $companion"
start=$(rpc ene.host.v1.WorkService/StartWork "{\"companionId\":\"$companion\",\"title\":\"sandbox\",\"instruction\":\"write the report\",\"clientId\":\"\"}")
echo "StartWork -> $start"

for _ in $(seq 1 60); do
  grep -q '^done$' "$run/cwd/report.txt" 2>/dev/null && break
  sleep 1
done
echo '=== report from inside the sandbox ==='
cat "$run/cwd/report.txt" 2>&1

proxy=$(grep -c '^proxy=http://127.0.0.1:3128' "$run/cwd/report.txt" 2>/dev/null || echo 0)
direct=$(grep -E '^direct=(000|[^0-9])' "$run/cwd/report.txt" 2>/dev/null | grep -vc '^direct=200' || echo 0)
via=$(grep -c '^via_proxy=200' "$run/cwd/report.txt" 2>/dev/null || echo 0)
[ "$proxy" = "1" ] && echo 'PASS  the agent was pointed at the proxy' || echo 'FAIL  HTTPS_PROXY is missing'
[ "$direct" = "1" ] && echo 'PASS  the agent has no direct route' || echo 'FAIL  the agent reached the internet without the proxy'
[ "$via" = "1" ] && echo 'PASS  the agent reached the internet through the proxy' || echo 'FAIL  the proxy did not carry the request'
echo '=== notices ==='
grep -iE "sandbox|proxy|acp" "$run/cwd/host.stderr" | tail -3
[ "$proxy" = "1" ] && [ "$direct" = "1" ] && [ "$via" = "1" ]
