# Coverage: desktop generation binding

guard-sha256: f1f136b3f9d191aad31d5dc29e023e5609b69f20e1343dd77a9412d73081a05c

Property: **an operation executes, and reports its result, only under the
activation generation this client currently holds.**
The harm if it breaks: a motion or eye-watch accepted for one pairing of
companion and client generation runs after the pairing moved to another device
or another generation, or a result is reported under a generation the host has
already replaced, so a stale device acts as if it still owns the character.

## What the check covers

| Check | Code it describes |
| --- | --- |
| `only_the_claim_naming_the_current_binding_executes` | `admits` in `apps/ene-desktop/src/session.rs` |
| third guard, held generation | `admits`'s revision arm; `refusal` is its only caller |
| `a_refusal_says_which_part_does_not_match` | the error text the client logs and shows |

`refusal` builds a `Claim` from `w::DeviceOperation` and calls `admits`, so
every dispatch the client executes passes through this one decision. Nothing
else in the client compares `activation_revision` except `subscribe_audio`,
`report_perception`, `capture_tick` and `report_playback`, which read
`binding.revision` rather than compare it to an incoming claim.

## The domain, in full

The test enumerates the product of:

- our client id: `desktop`, `""`
- our subscription id: `sub-1`, `""`
- the operation's client id: `desktop`, `other`, `""`
- the operation's subscription id: `sub-1`, `sub-2`, `""`
- the held binding: none, generation 1, generation 2
- the operation's generation: 0, 1, 2, 3

2 × 2 × 3 × 3 × 3 × 4 = 432 cases. Every distinguishable input of the decision
function is in that set, so this is a complete check of the guard, not a sample.
The test asserts the count, so widening the domain without updating the
expectation fails loudly.

## What it does not cover

- Ordering between the guard and the side effects. The guard runs before
  `execute`, and the result is sent with `operation.activation_revision`, but
  no check models the stream, the reconnect, or the window between the check and
  the report. The host's own property (`verify-ene-host-approval-binding`)
  covers the dispatch side.
- The empty-subscription case: while the client does not yet know its
  subscription id (before `DeviceReady`), the subscription guard is skipped by
  design, so only identity and generation apply. That is modelled, not flagged.
- The Connect layer, activation races between two clients, and the host's
  handling of an expired generation.
- Rust has no verifier that runs on this code for this class of property, so
  the check is an exhaustive test over generated inputs rather than a proof over
  unbounded state. The domain above is the bound.

## Keeping it in step

`guard-sha256` is the whitespace-normalised text of `fn admits`, hashed. It
changes whenever the function's body changes.

After editing `admits`, `refusal`, or the test:

1. Read the new function and decide whether the property still says what the
   code does.
2. Run `pwsh -File verification/desktop-generation-binding/run.ps1`.
3. Run the break check: replace the revision arm with `Some(_) => Ok(())`, run
   the same script, confirm it fails with a case printed, restore the arm. This
   is the same mutation the skill tells the next agent to use.
4. Update `guard-sha256` to the value `check.py` prints, and say in the commit
   message what you re-verified.

Never update the hash without step 1. A hash bump that skips it turns the
record into a rubber stamp.

## Current status

The exhaustive check passes on 432 cases and the break check fails as expected
(one case printed: `id "desktop"/"desktop" subscription "sub-1"/"sub-1"
revision Some(1)/0`). The source was last confirmed at HEAD `51b5a07` with the
client's working-tree changes uncommitted; the guard hash above is that state.
