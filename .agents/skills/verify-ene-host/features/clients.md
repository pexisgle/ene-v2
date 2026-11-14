# Client registration and device stream

A client is the device that renders and acts: the desktop app, the avatar
window, a phone. It declares capabilities (motion, eye watch, ...), the host
decides what it may use, and the device's live channel carries operations and
perception.

## Sub-features

- `ClientService/RegisterClient` — declare id, name, platform, capability names
  and capability specs (tool name, description, JSON schemas, approval mode,
  presets); returns the stored client with `revision` and the host's grants.
- `ClientService/SetCapabilities` — replace capabilities/specs at the current
  `revision`; stale revision is `aborted`.
- `ClientService/List` — stored clients with `connected` and
  `activeCompanions`.
- `ClientService/Activate` — move the active client for a companion
  (`expectedRevision`, CAS); requires an existing companion.
- `ClientService/DeviceStream` — the per-client live channel; opens with a
  `ready` frame.
- `ReportPerception`, `CompleteOperation`, `GetOperation` — perception,
  operation results and ledger reads.

## How to get to it (user POV)

The user installs/opens the desktop client on a second machine; it registers,
then becomes the active device for the companion, and the character is drawn
there. Moving the active client invalidates the previous generation's open
operations.

## Driving it with connect.py

```sh
py=.agents/skills/verify-ene-host/scripts

# declare the device
$py/connect.py rpc ene.host.v1.ClientService/RegisterClient \
  '{"id":"c1","name":"desk","platform":"linux","capabilities":["body.motion"],
    "capabilitySpecs":[{"name":"body.motion","toolName":"body_motion",
      "description":"move","inputSchema":"{\"type\":\"object\"}",
      "approval":"CAPABILITY_APPROVAL_CONFIRM"}]}'

$py/connect.py rpc ene.host.v1.ClientService/List '{}'

# the companion must exist first; activation itself needs no model binding
$py/connect.py rpc ene.host.v1.ClientService/Activate \
  '{"companionId":"<id>","clientId":"c1","expectedRevision":"0"}'

# replace capabilities at the current revision (the value RegisterClient returned)
$py/connect.py rpc ene.host.v1.ClientService/SetCapabilities \
  '{"clientId":"c1","revision":"1","capabilities":["body.motion","eye.watch"],
    "capabilitySpecs":[{"name":"body.motion","toolName":"body_motion",
      "description":"move","inputSchema":"{\"type\":\"object\"}"},
      {"name":"eye.watch","toolName":"set_eye_watch","description":"watch",
       "inputSchema":"{\"type\":\"object\"}"}]}'

# open the device channel; the first frame is the ready acknowledgement
$py/connect.py stream ene.host.v1.ClientService/DeviceStream '{"id":"c1"}' \
  --count 1 --timeout 3
```

Grants come from the host's environment at start:
`ENE_HOST_ALLOWED_CAPABILITIES=body.motion,eye.watch host.sh start`. Unset or
empty means nothing is granted (operations still need per-operation approval).

## Observable end state

- `RegisterClient` returns the client with `revision: "1"`, the capability
  names, and the supplied specs echoed back.
- `SetCapabilities` at the current revision returns `revision: "2"` with both
  capabilities; the same call with a stale revision returns 409 `aborted`.
- `List` returns the client; `grantedCapabilities` appears only when the host
  was started with a matching `ENE_HOST_ALLOWED_CAPABILITIES`.
- `DeviceStream` answers HTTP 200 and its first frame is
  `{"ready":{"subscriptionId":"<uuid>"}}`.
- `Activate` returns `revision: "1"` and `GetSnapshot` then reports
  `activeCompanionId` for that companion; on a host with no companion it returns
  404 `companion not found` (precondition missing, not a stream problem).
- The active companion can be created without a model assignment, so activation
  is drivable from an empty state; turns on that companion report
  `model_unavailable` until an assignment is bound.

## Gotchas

- `SetCapabilities` requires the client's current `revision`; it is an
  optimistic update, not a patch.
- `Activate` needs an existing companion (see [companions.md](companions.md));
  a companion with no model binding is enough for activation, but not for a
  chat turn. Device operations additionally need a granted capability, and the
  model must call the tool, so they still need provider credentials.
- `DeviceStream` with an unregistered id returns a Connect error frame, not an
  HTTP error: HTTP is still 200 because the stream is established before the
  handler validates. `connect.py` exits 1 when it sees the error frame.
- `PerceptionEvent` for a client that is not the active one is silently
  discarded by design (returns 200); that is not evidence the event was applied.
- `CompleteOperation` must arrive while the device stream is open. Closing the
  stream first makes the host mark the pending operation `uncertain`, and the
  late result is refused. Start the stream with `--count` large enough to stay
  open, read the operation frame, report the result, then stop the reader.
- `connect.py stream` prints each frame as it arrives, so a background stream
  can be polled while a unary call answers it. The exit status still means
  "all `--count` frames arrived" (0), "timed out first" (3), or "error frame"
  (1).
