# Discovery, auth and process lifecycle

The user's client finds a running host, proves it owns it, and loses the port
when the host stops. Everything else depends on this feature.

## Sub-features

- Private discovery: runtime directory 0700, `connection.json` 0600 with
  `endpoint` (`http://127.0.0.1:<port>/rpc`) and `token`.
- Owner Bearer auth on every RPC; no per-client credentials (`client_id` is
  identity for routing, not authentication).
- Transport policy: only loopback `Host` authorities and loopback `Origin`
  URLs; HTTP/2 prior knowledge (h2c) accepted.
- Single-instance exclusion: two hosts cannot share the same state or runtime
  directory (both are locked).
- Refusal of world-readable state/runtime directories.
- Graceful shutdown: SIGINT/SIGTERM closes streams, saves state, removes
  `connection.json`, exits 0.
- Token rotation: each start mints a new owner token.

## How to get to it (user POV)

Start the desktop client; the host starts as its child and the client reads the
runtime directory's `connection.json`. Stopping the app stops the host. A second
instance of the app must not take over a running host's state — it must fail
with a lock message instead.

## Driving it with connect.py

```sh
run=$(.agents/skills/verify-ene-host/scripts/host.sh start)
export ENE_HOST_CONNECTION=$run/runtime/connection.json
.agents/skills/verify-ene-host/scripts/host.sh doctor   # all PASS

# negative auth / transport checks (curl carries the header explicitly)
endpoint=$(python3 -c 'import json,os;print(json.load(open(os.environ["ENE_HOST_CONNECTION"]))["endpoint"])')
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$endpoint/ene.host.v1.HostService/GetSnapshot" \
  -H 'Content-Type: application/json' -d '{}'                       # 401
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$endpoint/ene.host.v1.HostService/GetSnapshot" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer wrong" -d '{}'   # 401
curl -sS -o /dev/null -w '%{http_code}\n' -X POST "$endpoint/ene.host.v1.HostService/GetSnapshot" \
  -H 'Content-Type: application/json' -H "Authorization: Bearer <token>" -H 'Host: evil.example' -d '{}'  # 401

# second instance on the same directories must fail, not steal the lock
ENE_HOST_STATE_DIR="$run/state" ENE_HOST_RUNTIME_DIR="$run/runtime" \
  timeout 20 target/debug/ene-host; echo "exit=$?"                  # non-zero, "already in use"
```

## Observable end state

- `doctor` reports the pid alive, `connection.json` exists at 0600, both
  directories at 0700, and `GetSnapshot` 200 with `health.ready = true`.
- The negative checks above return 401; the same call with the published token
  returns 200 and `transport: "/rpc"`.
- A second process started against the same directories exits non-zero and its
  stderr contains `already in use`.
- After `host.sh stop`, the recorded pid is gone and `connection.json` was
  removed; the evidence copy under `<run>/evidence/connection.json` still
  exists.
- `.agents/skills/verify-ene-host/scripts/host.sh restart` publishes a new token
  on the same state, and the old token returns 401.

## Gotchas

- The host creates `state/` and `runtime/` itself. Pre-creating them with
  `mkdir -p` can leave them 0755 (umask 022) and the host then refuses to start
  with `Error: InsecurePermissions { expected: 448, actual: 493 }` (448 =
  0700, 493 = 0755). `host.sh start` deliberately does not pre-create them.
- `connection.json` is deleted on graceful exit; copy it before stopping if the
  token or endpoint is part of the proof (`host.sh start` already copies it).
- `host.lock` files are intentionally left behind for reuse; their presence is
  not a stale lock.
- The lock covers both directories: a second host with only the state directory
  shared still fails.
