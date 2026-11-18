# Notifications: subscribe, cursor, live events

Every client keeps one `HostService/Subscribe` stream open to stay live:
approval cards, activation moves, client capability changes and device
operations arrive on it.

## Sub-features

- `Subscribe` opens with an `initial_state` notification carrying the full
  snapshot and a cursor (`epoch` + `seq`).
- Live notifications after that carry `kind`, event payload and increasing
  `cursor.seq`.
- Resuming with a cursor from the same epoch replays the ring; a stale cursor
  or a gap produces `resync_required` instead of silent loss.
- `client_id`-targeted notifications (`client_updated`) go only to that
  client's subscriber, not to every subscriber.

## How to get to it (user POV)

The desktop client subscribes on startup and renders from the stream. When the
user changes a device's capabilities, the device list updates without a
refresh; that update is a notification.

## Driving it with connect.py

A live event needs a second process. The example registers a client, subscribes
as that client, then bumps its capabilities and reads the resulting frame.

```sh
py=.agents/skills/verify-ene-host/scripts

$py/connect.py rpc ene.host.v1.ClientService/RegisterClient \
  '{"id":"c1","name":"desk","platform":"linux","capabilities":["body.motion"],
    "capabilitySpecs":[{"name":"body.motion","toolName":"body_motion",
      "description":"move","inputSchema":"{\"type\":\"object\"}",
      "approval":"CAPABILITY_APPROVAL_CONFIRM"}]}'

mkdir -p "$run/evidence/notifications"
$py/connect.py stream ene.host.v1.HostService/Subscribe '{"clientId":"c1"}' \
  --count 2 --timeout 5 > "$run/evidence/notifications/frames.jsonl" 2>&1 &
stream_pid=$!
sleep 0.7
$py/connect.py rpc ene.host.v1.ClientService/SetCapabilities \
  '{"clientId":"c1","revision":"1","capabilities":["body.motion","eye.watch"],
    "capabilitySpecs":[{"name":"body.motion","toolName":"body_motion",
      "description":"move","inputSchema":"{\"type\":\"object\"}"},
      {"name":"eye.watch","toolName":"set_eye_watch","description":"watch",
       "inputSchema":"{\"type\":\"object\"}"}]}'
wait $stream_pid; echo "stream exit=$?"
cat "$run/evidence/notifications/frames.jsonl"
```

Both responses must be 200; the stream exits 0 after two frames.

## Observable end state

- Frame 1: `"kind":"initial_state"` with `initialState.serverVersion`, a
  `companion` list (absent while empty), and `cursor.epoch`.
- Frame 2: `"kind":"client"` with `clientUpdated.client.capabilities` equal to
  the new set, and `cursor.seq` exactly one greater than frame 1's.
- A subscriber with no `clientId` also receives the `initial_state` but not the
  `client_updated` frame (it is targeted).

## Gotchas

- The stream body is not plain JSON: it is one Connect envelope (flag byte,
  4-byte big-endian length, JSON). Use `connect.py`; hand-written curl needs
  `--data-binary` with a pre-built frame.
- `Subscribe` with a `clientId` that is not registered returns a Connect error
  frame (`not_found: client not found`); register first.
- `--count` stops the client after N frames, which closes the stream. For a
  long-lived subscription use a larger count or `--timeout`.
- A cursor from a different host run has a different epoch; do not expect
  replay across restarts. Expect `initial_state` instead.
- `SetCapabilities` needs the client's current `revision`; a stale value is
  409 `aborted` and produces no notification.
