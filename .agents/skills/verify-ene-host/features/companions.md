# Companions and their mind profile

A companion is the character the user talks to. The desktop editor creates,
renames and deletes them; the head agent's personality, mind state and profile
hang off the companion row.

## Sub-features

- `HostService/CreateCompanion` — name, mind, body/voice documents, model
  assignments, profile, persona.
- `HostService/ListCompanions` / `GetCompanion` / `UpdateCompanion` (optimistic
  revision; stale revision is `aborted`) / `DeleteCompanion`.
- `HostService/GetMind` and `UpdateMindProfile` — the stored mind view and user
  profile.
- Every create/update republishes a companion agent and its model binding;
  deletion retires it.

## How to get to it (user POV)

The user opens the companion editor, creates a character with a name, then
talks to it. Creating a companion is the first-run path: after it exists, a
client activation and the chat surface become meaningful.

## Driving it with connect.py

The user's journey starts with a create, and a model assignment is not
required: the companion is stored first and the model binding is derived later,
so a fresh host with no LLM connection can still create, rename and delete
companions. Assignments name a connection and a model (`assignments.companion`);
until one is set the companion answers turns with a `model_unavailable` notice
rather than failing the write.

```sh
py=.agents/skills/verify-ene-host/scripts

# the user's create path
$py/connect.py rpc ene.host.v1.HostService/CreateCompanion '{"name":"Mio","mind":"kind"}'

# read back through a different call
$py/connect.py rpc ene.host.v1.HostService/ListCompanions '{}'
$py/connect.py rpc ene.host.v1.HostService/GetCompanion '{"id":"<id>"}'

# optimistic update: pass the revision returned by the create
$py/connect.py rpc ene.host.v1.HostService/UpdateCompanion \
  '{"id":"<id>","revision":"1","name":"Mio Renamed"}'

# persistence: restart on the SAME state, then list again
$py/host.sh restart <run-id>

# delete and confirm it is gone
$py/connect.py rpc ene.host.v1.HostService/DeleteCompanion '{"id":"<id>"}'
$py/connect.py rpc ene.host.v1.HostService/ListCompanions '{}'
```

Capture the create response, the list response, the row count from
`<run>/state/host.sqlite3` (`SELECT count(*) FROM companions`) before the
delete, and the list after `host.sh restart` (which keeps state; `host.sh stop`
would delete it and prove nothing).

## Observable end state

- Create returns the companion with `revision: "1"` and an id.
- `ListCompanions` contains it, and SQLite `companions` has one more row.
- Update with the current revision returns `revision: "2"`; update with a stale
  revision returns 409 `aborted`.
- `host.sh restart` (same run directory) publishes a new token and the
  companion is still in `ListCompanions` with its updated name and revision;
  a clean stop after that exits 0.
- Delete returns `{}`, the list returns `{}`, and SQLite `companions` is back
  to zero.

## Gotchas

- An unbound model is a normal state, not a failure: create/update succeed and
  the host emits a `model_unavailable` notice until an assignment can be bound.
  A **storage** failure still fails the write and the start — do not treat a
  503 on create as expected.
- History: until `51bdd59`, a create without an assignment returned 503 after
  inserting the row and the next start exited 1 (`no model is assigned to this
  role`). If you see that on an older checkout, the feature is broken there,
  not in this harness.
- `assignments` is optional in the proto, so a client that creates first and
  assigns later is within the contract.
- `host.sh stop` deletes `state/` and `runtime/`; use `host.sh restart` for the
  persistence leg, or the second list will be empty for the wrong reason.
- `GetMind`/`UpdateMindProfile` need a companion id and are read/write on the
  mind store; they do not need a model either, so they can be appended to the
  same journey.
