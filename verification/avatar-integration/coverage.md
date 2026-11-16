# Coverage: avatar integration

Property: **what the desktop client asks the avatar renderer to do actually
happens, and the client's own request parsing matches what it declares.**

The harm if it breaks: a companion smiles and the face never moves, the mouth
does not follow its voice, a motion change reloads the model (a visible freeze
and a lost placement), or two clips run at full weight at once so the pose is a
mixture of two animations.

## What `run.ps1` proves

It starts the real `ene-avatar` binary, speaks its JSON Lines protocol over
stdin/stdout, loads a real VRM/VRMA from this machine, and reads back `get`.
No mocked renderer is involved.

| Check | Code it describes |
| --- | --- |
| the model loads | `character::update`, `scene_ready` |
| expression is accepted / reported | `control::Command::Expression`, `Character::expression` |
| the expression fades in | `character::expressions`, `ene-vrm` `ExpressionWeights` |
| lip_sync reaches the mouth | `control::Command::LipSync`, `character::expressions` |
| silence closes the mouth | the release half of `character::expressions` |
| a motion change is applied | `control::Command::Set`, `character::motion_change` |
| a motion change does not reload the model | `Pending::Fade` path, no `reload` |
| a motion change fades / settles on one clip | `start_main`, `motion_change`, `AnimationTransitions` |
| no motion returns to rest | `Pending::Rest`, the `running` rule in `update` |
| the client wire format tests pass | `avatar::expression_request`, `avatar::lip_sync_line` |
| the client motion plan tests pass | `MotionPlan::parse` in `session.rs` |

The script found a real defect when it was written: the first clip was started
with `AnimationPlayer::play` instead of through `AnimationTransitions`, so the
first motion change had no old animation to fade out and both clips stayed at
full weight forever. `the fade settles on one clip` is the check that catches
it.

## What it does not cover

- The client's two calls (`Avatar::show`, `Avatar::expression`) are not driven
  here; the wire builders and the request parsing they use are, and the renderer
  accepts exactly those shapes. Driving the whole client needs a model that
  calls `body.motion`, which is the model's decision, not a fixed input.
- Pixel output. Whether a face looks happy is not asserted; the morph weights
  reaching the model are (ene-vrm's unit tests) and the state is (this script).
- Audio. The script sends the amplitudes a client would send; it does not open
  an audio device or play a buffer.
- CPU or frame rate under a real model, and the placement of the window on a
  multi-monitor desktop.
- Linux. The script is PowerShell and drives the Windows build.

## Requirements

- Windows, a display, and a GPU that can open the renderer's window.
- A VRM and two VRMA files. The defaults are the ones the desktop client's
  asset cache uses (`%LOCALAPPDATA%\ene\cache`). Override with
  `ENE_VERIFY_MODEL`, `ENE_VERIFY_MOTION`, `ENE_VERIFY_MOTION2`.
- A built workspace; the script runs `cargo build -p ene-avatar` itself.

## Running

```powershell
pwsh -File verification/avatar-integration/run.ps1
```

Exit 0 means every check passed. Exit 1 means a check failed or an asset is
missing, and the failing check names itself.
