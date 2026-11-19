# Coverage: desktop device binding

guard-sha256: db8fb68c1eb72c77be0a674b33b61c3603853b9e05e1511da6e48282df2843da

Property: **an operation executes, and reports its result, only when it names
this client's identity and current device stream, and either names a work
session bound to this client or this client is the active device for a
companion.**
The harm if it breaks: a motion or eye-watch addressed to another client, or sent
on a device stream this client has already replaced, runs anyway, so a stale or
wrong device acts as if it owns the character.

## What the check covers

| Check | Code it describes |
| --- | --- |
| `only_the_claim_naming_the_current_binding_executes` | `admits` in `apps/ene-desktop/src/session.rs` |
| third guard, active device | `admits`'s binding arm; `refusal` is its only caller |
| `a_refusal_says_which_part_does_not_match` | the error text the client logs and shows |

`refusal` builds a `Claim` from `w::DeviceOperation` and calls `admits`, so
every dispatch the client executes passes through this one decision. The host no
longer carries an activation generation on the wire (`docs/design/voice-avatar.md`,
`Activation`), so the client's claim is identity plus device stream, and the
binding is a yes/no rather than a generation comparison.

## The domain, in full

The test enumerates the product of:

- our client id: `desktop`, `""`
- our subscription id: `sub-1`, `""`
- the operation's client id: `desktop`, `other`, `""`
- the operation's subscription id: `sub-1`, `sub-2`, `""`
- whether we hold the companion: no, yes
- the operation's work id: `None`, `Some("")`, `Some("work-1")`

2 × 2 × 3 × 3 × 2 × 3 = 216 cases. Every distinguishable input of the decision
function is in that set, so this is a complete check of the guard, not a sample.
The test asserts the count, so widening the domain without updating the
expectation fails loudly.

## What it does not cover

- Ordering between the guard and the side effects. The guard runs before
  `execute`, but no check models the stream, the reconnect, or the window between
  the check and the report. The host's own property
  (`verify-ene-host-approval-binding`) covers the dispatch side.
- The empty-subscription case: while the client does not yet know its
  subscription id (before `DeviceReady`), the subscription guard is skipped by
  design, so only identity and the binding apply. That is modelled, not flagged.
- The Connect layer and activation races between two clients.
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
3. Run the break check: disable the identity arm with `if false && ...`, run the
   same script, confirm it fails with a case printed, restore the arm.
4. Update `guard-sha256` to the value `check.py` prints, and say in the commit
   message what you re-verified.

Never update the hash without step 1. A hash bump that skips it turns the
record into a rubber stamp.
