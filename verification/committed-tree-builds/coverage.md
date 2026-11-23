# Coverage: the committed tree builds

Property: **every commit builds on its own, not just the working tree.**

The harm if it breaks: a commit that references a file, function or re-export it
did not include. The working tree still compiles, because the missing piece is
sitting unstaged, so every local gate passes and `main` is broken for anyone who
checks it out. This happened once here: the risk badge landed in one commit and
the re-exports it needed were left unstaged, so `HEAD` failed with
`unresolved import ene_client_core::domain::RiskLevel` while `cargo clippy` on
the working tree was clean.

## What `run.sh` proves

It checks the commit out into a fresh git worktree and runs the full clippy gate
there, so nothing unstaged can satisfy a reference.

| Check | Code it protects |
| --- | --- |
| the commit builds with `-D warnings` | every crate, at the commit's own tree |

## What it does not cover

- It does not run the test suite. Tests are slower and this check exists for
  the compile-time linkage that the working tree hides.
- It builds `--all-features`, which is what CI's clippy job runs, not every
  feature combination.

## Running it

```sh
verification/committed-tree-builds/run.sh          # HEAD
verification/committed-tree-builds/run.sh <rev>
```
