---
name: verify-ene-host-approval-binding
description: Check that ene-host binds a confirm-gated device operation to the client generation the user approved it for, across approve/move/dispatch/result orderings. Use when changing the permission gate, approval submission, client activation, device dispatch or device result handling in apps/ene-host, or when a change could let consent, operations or results cross an activation generation.
---

# Verify ene-host approval binding

**The promise (plain words):** a device operation approved for one client
generation only ever runs on that generation, only after the user approved it,
and only that generation's result can complete it.
**The harm if it breaks:** a motion, eye-watch or other device action the user
approved for one device runs on another device, or a late result from an old
device rewrites a fresh operation.

The check is a TLA+ model (`verification/approval-binding/ApprovalBinding.tla`)
of the gate → approval → dispatch → result pipeline, model-checked with TLC
over every ordering of those steps, plus a text-and-order check that the model
still matches the Rust guards it describes.

Tool choice: the question is "can these events happen in a bad order", which is
what a model checker answers over the whole interleaving space; no verifier runs
on this Rust ECS code for that class of property. TLA+/TLC comes from nix
(`nixpkgs#tlaplus` and a JRE), so it installs nothing globally and is cached
after the first run.

## When to run it

Run it when a change touches any row of the coverage table in
`verification/approval-binding/coverage.md`, and in particular:

- `apps/ene-host/src/host/permission/mod.rs` — `gate`, `submit`,
  `invalidate_generation`
- `apps/ene-host/src/host/client/mod.rs` — `activate`
- `apps/ene-host/src/host/client/device.rs` — `accept`, `dispatch`, `complete`,
  `resend`, `disconnected`
- `apps/ene-host/src/host/db/approvals.rs`, `db/clients.rs` — expiry and
  operation failure queries

A change that only touches unrelated handlers does not need it.

## Run it

```sh
direnv exec . bash verification/approval-binding/run.sh
```

Exit 0 means: the sync check found every modelled guard in the code and in the
expected order, and TLC found no reachable state that violates the four
invariants. The TLC log is `verification/approval-binding/.out/check/tlc.log`.

Prove the checks can fail (run after changing the model or adding a guard):

```sh
direnv exec . bash verification/approval-binding/run.sh --mutants
```

Each mutant weakens exactly one modelled guard and must make its named
invariant fail. Four mutants ship: no expiry on move, no dispatch
revalidation, dispatch before approval, result of any generation.

## Read a result

- **Pass:** the four properties hold for every ordering inside the model's
  bounds (see `coverage.md` for the bounds). This covers the design, not the
  compiled code.
- **`FAIL sync: guard missing/changed`:** the Rust code no longer contains the
  check the model relies on. The verification is stale or the guard was
  deleted; do not edit the model until you have read the function and know
  which it is. A deleted guard is a **finding**, not a modelling error; report
  it with the file and line, do not re-add it silently.
- **`Invariant ... is violated` / TLC error:** the log ends with a trace
  (`State 1 ... State N`). Read that trace with the pstack **`/statechart`**
  skill — it turns an invariant violation into the shortest sequence of steps
  in plain words. Then map each state back to the action comment in the `.tla`
  file and to the Rust function it names. If the trace's step order is
  something the host cannot actually produce (for example a submission before
  the approval exists), it is a modelling error: fix the model, not the code.
  If the order is producible, stop and report the counterexample.
- **`WARN sync: covered code changed after <sha>`:** a commit after the recorded
  `sync-commit` touched one of the covered files. Re-read those functions; if the
  model still describes them, update `sync-commit` in `coverage.md` and say what
  you verified. A commit that touches only unrelated files does not warn.

## What is checked

| Invariant | Plain meaning |
| --- | --- |
| `NoStalePendingApproval` | While an approval is pending, its generation is still the active one (no consent card survives a move). |
| `NoUnapprovedDispatch` | No device operation exists before the user approved its call. |
| `DispatchedToCurrentGeneration` | The operation is written and dispatched only to the generation active at dispatch — the approve-then-move race. |
| `AppliedBySameGeneration` | Only the operation's own client revision can complete it; a late result from another generation is discarded. |

## Bounds and limits

- Two clients, one companion, one move (`MaxRev = 1`), one confirm-gated
  capability, one call.
- Serialized API-call orderings; not thread-level races (the host's ECS is
  single-threaded).
- Not covered: the Connect/REST layer, approval-hash canonicalisation, the AUTO
  policy path, work-session approvals, reconnect/resend, timeouts.
- `SubmitAfterMove` (the submit-time recheck) is unreachable in the base model
  because a move already expires pending approvals; it is exercised by the
  `move-no-expiry` mutant as defense in depth.

## Keeping it in step

`verification/approval-binding/coverage.md` records which functions each model
action describes and the commit at which that was last confirmed. After
editing a covered function: adjust the model action, rerun both commands,
verify each mutant is still caught, then move `sync-commit` forward. Never move
it without re-reading the code.

`check_sync.py` enforces the textual and ordering half automatically; a
semantic change that keeps the guard text intact still needs a human read of
the function.

## Current status

At `sync-commit` `51bdd595cf1fcf7e9f00c5b5de9902b500529c1a` the model passes
(30 distinct states, complete search) and all four mutants are caught. No
finding is open for this property. A separate, unrelated defect exists in
companion creation and startup (see the `verify-ene-host` skill's
`features/companions.md`); it does not affect these invariants.
