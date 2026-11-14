---
name: verify-ene-host
description: Drive the ene-host process (apps/ene-host, the loopback Connect RPC API that the desktop client and hands talk to) and prove a change works against the real binary. Use when changing ene-host RPC handlers, discovery/auth, companion/mind/settings persistence, notifications, or client device streams, and when a claim about host behavior needs live evidence instead of unit tests.
---

# Verify ene-host

`apps/ene-host` is a headless process with a loopback Connect RPC API. The
desktop UI, the voice pipeline and the device-integrating clients are separate
programs; in this repo the host itself is the user-facing surface you can drive
from an agent. Everything below runs against a real `target/debug/ene-host`
process with its own state directory.

The API contract is `crates/ene-rpc/proto/host.proto` (canonical per
`docs/api.md` §7). Requests are Connect unary JSON
(`Content-Type: application/json`) or Connect streaming
(`Content-Type: application/connect+json`, one length-prefixed frame per
message). The shipped `scripts/connect.py` speaks both; use it instead of
hand-rolling the envelope.

## Launch

Build once, then start an isolated instance. The host publishes
`connection.json` with `endpoint` and `token` into its runtime directory; it
creates that directory itself and refuses a pre-existing world-readable one.

```sh
run=$(.agents/skills/verify-ene-host/scripts/host.sh start)
export ENE_HOST_CONNECTION=$run/runtime/connection.json
```

`host.sh start` builds the binary (incrementally, every time, so a stale
binary can never stand in for the change under test; `VERIFY_SKIP_BUILD=1`
opts out), then blocks until the host is *usable*: `connection.json` exists,
is 0600, and an authenticated `GetSnapshot` answers 200 with
`health.ready = true`. It prints the run directory. Every run gets a fresh
state/runtime pair under `target/verify-ene-host/runs/<run-id>/`, and the host
binds `127.0.0.1:0`, so concurrent runs never share a port or a lock.

`host.sh restart` re-runs the host on the **same** state and publishes a new
token; it is the only supported way to prove that something survived a
restart. `host.sh stop` sends SIGTERM to the pid it recorded, waits, and
deletes `state/` and `runtime/` — after `stop` there is no state to restart
from, by design (teardown is not a restart).

Do not point `ENE_HOST_STATE_DIR` at a real user's directory (the default is
`~/.local/state/ene`), and do not drive an existing host started by the desktop
app: it owns a different API contract surface and may hold a lock this harness
would contend with.

## Doctor

Run this first whenever a drive fails or a run may be stale. It is read-only:
process liveness by pid, discovery file and permissions, and an authenticated
snapshot.

```sh
.agents/skills/verify-ene-host/scripts/host.sh doctor <run-id>
```

Every line is PASS or FAIL; exit status is non-zero on any FAIL. Doctor never
restarts or repairs; if it fails, `stop` the run and start a fresh one. A host
that exited (for example a startup failure you are investigating) fails doctor
on the pid and connection lines while its `host.stderr` still holds the reason.

## Drive

The harness is `scripts/connect.py` (stdlib only). It reads the connection file
from `--connection` or `ENE_HOST_CONNECTION`, prints each response message as
one compact JSON line on stdout, and the HTTP status on stderr.

```sh
# unary: full proto service + method
.agents/skills/verify-ene-host/scripts/connect.py rpc ene.host.v1.HostService/GetSnapshot '{}'
.agents/skills/verify-ene-host/scripts/connect.py rpc ene.host.v1.SettingsService/ListProviders '{}'

# server stream: collect N frames (default 1), bounded by --timeout seconds
.agents/skills/verify-ene-host/scripts/connect.py stream ene.host.v1.HostService/Subscribe '{}' \
  --count 1 --timeout 3
```

Exit codes: `0` success (HTTP 200, and N frames for a stream), `1` HTTP or
protocol error, `2` usage, `3` stream timed out before N frames. A stream that
ends with a Connect `error` frame exits `1`.

Live notifications need a second process: start the stream in the background
with `--count N`, then trigger the unary call, then `wait`. See
[features/notifications.md](features/notifications.md) for the exact
sequence.

For negative auth checks the helper is deliberately not enough (it always
sends the good token); use curl against the published endpoint:

```sh
endpoint=$(python3 -c 'import json,os;print(json.load(open(os.environ["ENE_HOST_CONNECTION"]))["endpoint"])')
curl -sS -w '\nHTTP %{http_code}\n' -X POST "$endpoint/ene.host.v1.HostService/GetSnapshot" \
  -H 'Content-Type: application/json' -d '{}'
```

## Evidence

Proof goes under `<run>/evidence/<feature>/`, which `host.sh stop` never
deletes. Capture at least:

- the request and full response JSON (stdout of `connect.py`, stderr for status),
- for streams, the frame JSONL **and** the frame order (`kind` and
  `cursor.seq`), because a replay bug shows up as a gap, not a missing message,
- the side effect, read back independently: `HostService/ListCompanions` after a
  create, `SettingsService/ListPolicyRules` after a policy write, or SQLite at
  `<run>/state/host.sqlite3` (`PRAGMA user_version`, row counts) after a
  persistence claim,
- `host.sh doctor` output taken after the action,
- `host.stderr` when behaviour differs from the contract.

Proof standards: drive the same RPC a user's client would call, with the same
arguments, and check the resulting state through a different call than the one
that wrote it. Do not call internal helpers or test-only paths. Capture the
action *and* the resulting state, not only the final screen. Do not weaken a
journey to make it pass: a journey blocked by a missing precondition is
reported as failed with the reason, and that reason belongs in the feature's
`Gotchas`.

## Cleanup

- Only kill the pid recorded in `<run>/host.pid`; never kill by process name and
  never `pkill -f ene-host` (it matches the shell that typed the command).
- `.agents/skills/verify-ene-host/scripts/host.sh stop` is the supported
  teardown. Run it after every attempt, including failed ones, so no process,
  port or lock is stranded. Never use `stop` in the middle of a journey you
  want to survive: it deletes the state under test.
- Evidence survives teardown. Scratch state does not.

## Helpers

| File | Invocation | What it does |
| --- | --- | --- |
| `scripts/host.sh` | `host.sh {start\|restart\|doctor\|stop\|path} [RUN_ID]` | One isolated instance: incremental build, wait-until-usable, pid tracking, state-preserving restart, scratch cleanup |
| `scripts/connect.py` | `connect.py {rpc\|stream} SERVICE/Method '<json>' [--count N] [--timeout S]` | Connect unary and streaming client, stdlib only |

`RUN_ID` defaults to a timestamp. Override the run root with `VERIFY_ROOT` or
the whole directory with `VERIFY_RUN_DIR`.

## Journeys

[journeys/](journeys/README.md) holds end-to-end runs that need a real binary,
real providers and network: contract coverage, work runs and approvals, a chat
turn, schedule delivery, the device stream, MCP, TTS, STT, the ACP sandbox and
restart behaviour. Run `journeys/all.sh` for the whole set, or one file for the
feature you touched. Each run writes its log to `target/verify-ene-host/`.

## What this cannot prove

- Mind consolidation and compaction against a real provider. The unit tests
  drive them with rig's mock model; the journeys do not.
- The avatar renderer and the ACP protocol against a real external agent. The
  journeys use a reporting child for the sandbox and a scripted client for the
  device stream.
- Long-run behaviour: leaks, load and multi-day uptime.

## Feature map

Start from [features/README.md](features/README.md). Cover the feature the
change touches; one journey is enough to prove the harness, the map exists so
every user-facing path can be covered over time.
