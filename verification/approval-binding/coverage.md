# Coverage: approval → dispatch → result generation binding

sync-commit: `60ba55ba3ea0d3e8821deca3745204568c1d3e34`

## The promise

A confirm-gated device operation belongs to one client generation: the client
that was active when the user approved the call, at that activation revision.
The host must never dispatch it to another generation, never run it without the
user's approval, and never let a result from another generation complete it.

Why it matters: these are the user's device actions (motion, eye watch, ...).
If consent leaks across a generation, an operation the user approved for one
device runs on another, or a stale result rewrites a fresh operation. Unit
tests cover single moves and single results; they do not cover the orderings.

Four properties, each checked by TLC over every reachable state:

1. `NoStalePendingApproval` — while an approval is pending, its generation is
   still the active one. A pending card for a device that is no longer active
   would be a consent leak waiting for a click.
2. `NoUnapprovedDispatch` — a device operation exists only after its approval
   was granted. Running a held call early would act before the user answered.
3. `DispatchedToCurrentGeneration` — an operation is written and dispatched only
   to the generation that is active at dispatch. This is the race that matters:
   approve, then move the active client before the tool call reaches the device
   layer.
4. `AppliedBySameGeneration` — only a result from the operation's own client
   and revision completes it. A late result from a moved client must be
   discarded, not applied.

The covered guards were re-read at the sync commit after cached-statement,
Serde-enum and direct-error-constructor migrations. Their generation comparisons,
SQL predicates and mutation order are unchanged. The unmodified sync checker and
base model passed. Each mutant produced its expected invariant violation.
The [audit record](<../../docs/refactoring/library-delegation-audit.json>) retains
the verification evidence and its limits.

Real HTTP/2 device streams also verified current-generation consent and completion,
expired consent after a move, and refusal of old-client or old-revision results.
The public API did not force a move between approval and dispatch. That ordering
has bounded model coverage only. Gate re-evaluation after a move can create a new
consent card, which the single-approval model does not represent. Approving the
observed replacement card did not dispatch the already-refused call.

## Covered code

| Code | What the model takes from it |
| --- | --- |
| `apps/ene-host/src/host/permission/mod.rs` — `gate` | A confirm-gated call binds `CallBinding{client_id, activation_revision}` at gate time and creates the `Approval` with the same pair plus the canonical argument hash (`ApprovalRequest`), then holds the call. Modelled as `ToolCall`. |
| `apps/ene-host/src/host/permission/mod.rs` — `submit` | Re-checks `bound_generation` against the approval's pair, expires on mismatch and refuses with `client_moved`; otherwise marks the call `Approved`. Modelled as `SubmitWhenCurrent` / `SubmitAfterMove` / `SubmitDeny`. |
| `apps/ene-host/src/host/client/mod.rs` — `activate` | CAS-bumps the activation revision, then `fail_operations_of(old_client, previous.revision)` and `expire_approvals_for(companion)`, then `permission::invalidate_generation`. Modelled as `Move`. |
| `apps/ene-host/src/host/client/device.rs` — `accept` | Revalidates `store.target(...)` against the call's binding immediately before `insert_operation`; a move yields `client_moved` and writes nothing. Modelled as `DispatchChecked` / `DispatchRefused`. |
| `apps/ene-host/src/host/client/device.rs` — `complete` | Discards a result whose `client_id`/`activation_revision` differ from the stored operation, before any state change. Modelled as `ResultSame` (a mismatched result is a no-op). |
| `apps/ene-host/src/host/db/approvals.rs` — `expire_approvals_for` | Pending approvals of the companion become `expired` on a move. |
| `apps/ene-host/src/host/db/clients.rs` — `fail_operations_of` | `pending`/`dispatched` operations of the old generation become `failed` on a move. |

`check_sync.py` fails when one of these guards disappears or changes order
relative to the write it protects.

## Bounds and assumptions

- Two clients, one companion, at most one activation move (`MaxRev = 1`), one
  tool call, one approval, one device operation. The model explores every
  ordering of those steps.
- The capability is confirm-gated (`CAPABILITY_APPROVAL_CONFIRM`); the AUTO path
  shares `accept`'s revalidation but is not modelled.
- `SubmitDeny` is modelled but not part of the four invariants.
- Timeouts, disconnects, reconnects and the resend path are not modelled; a
  reconnect cannot move a generation, and its revision filter is a separate
  guard.
- Work-session approvals (`work_id` set) are not modelled: they follow the
  session's stored generation rather than the companion activation.
- Serialization is assumed: the host runs the ECS on one thread, so the model's
  steps are the API calls in any order, not truly concurrent memory accesses.

## What it does not cover

- The code itself. TLC checks the model; `check_sync.py` checks that the model
  and the Rust guards still agree textually and in order. Neither proves the
  Rust compiler emitted the intended checks.
- The REST/Connect layer, the database's transactional behaviour, or the
  approval-hash canonicalisation (`canonicalise` in `permission/mod.rs`).
- The AUTO / policy-rule decision function; see `settings-and-policy.md` in the
  verify-ene-host skill for that surface.

## Keeping it in step

When you edit a covered function:

1. Re-read the function, then adjust `ApprovalBinding.tla` so each model action
   still matches it (update the comment mapping the action to the code).
2. `verification/approval-binding/run.sh` — sync check and TLC must pass.
3. `verification/approval-binding/run.sh --mutants` — every mutant must still be
   caught; add a mutant if you added a guard.
4. Move `sync-commit` above to the commit you confirmed, and say what changed.
   `check_sync.py` warns only when a **covered file** changes after that commit;
   an unrelated commit does not disturb the record.
