Read README.md carefully and follow the development instructions provided there accordingly.

## Structural & Code Quality Rules

- Keep every `.rs` file concise (target <= 500 lines, hard limit < 1,000 lines) and every function <= 120 lines (cognitive complexity <= 20, arguments <= 6). Split by responsibility when approaching limits.
- Do not place Protobuf conversions (`w::*` mutations), vendor/adapter branching, or RPC orchestration inside `apps/ene-desktop/src/ui/`.
- Do not introduce `Result<_, String>` in new code; define structured domain errors with `thiserror`.
- Any `#[allow(clippy::...)]` attribute must include an immediately preceding comment explaining why it is required and the condition for removing it.
- Keep crate dependencies strictly acyclic and unidirectional:
  - `apps/*` (`ene-host`, `ene-desktop`, `ene-avatar`)
  - -> Layer 2 (`ene-client-core`, `ene-plugin-runtime`, `ene-store`, `ene-voice`, `ene-work`, `ene-vrm`)
  - -> Layer 1 (`ene-domain`, `ene-policy`, `ene-mind`, `ene-catalog`, `ene-rpc`)
  - -> Layer 0 (`ene-plugin`, `ene-avatar-ipc`)

## Layered Architecture

Dependencies always point inward. The canonical four-layer model
([`docs/refactoring/ene_v2_refactoring_plan.md`](docs/refactoring/ene_v2_refactoring_plan.md) §2.1):

| Layer | Crates | May depend on | Never |
| :--- | :--- | :--- | :--- |
| **L0 Pure domain** | `ene-domain`, `ene-policy`, `ene-catalog`, `ene-mind`, `ene-work`, `ene-voice`, `ene-plugin` | std, serialization helpers, lower L0 crates | `ene-rpc`, `rusqlite`, `reqwest`, `axum`, `gpui`, `bevy_ecs`, OS FFI |
| **L1 Application** | `ene-host-core`, `ene-client-core` | L0 only; external I/O through port traits | concrete DB/HTTP/RPC/Win32 implementations |
| **L2 Infrastructure** | `ene-rpc`, `ene-store`, `ene-plugin-runtime`, `ene-vrm`, `ene-avatar-ipc`, `ene-platform-os` | L0, L1 port traits, external I/O crates | lateral adapter dependencies, business rules |
| **L3 Thin apps** | `apps/ene-host`, `apps/ene-desktop`, `apps/ene-avatar` | L0-L2 wiring, CLI, server startup, GPUI drawing | domain logic, direct `unsafe` |

## Module Placement

One rule decides where code goes: **pure model, algorithm, state machine and
policy evaluation belong in `crates/*`; I/O wiring, task startup, protocol
conversion and screen drawing belong in `apps/*`.** A rule that both a test and
the UI need is a domain rule and must not be re-implemented in the app.

- Target 350 lines per production file; 500 is the split threshold and 1,000 the hard cap.
- Inner modules default to `pub(crate)`; only what `lib.rs` re-exports is a public contract.
- Represent illegal states as impossible: prefer a state `enum` over
  co-varying `bool`/`Option` fields (for example `HostLifecycle`, not
  `failed: Option<String>` plus `closing: bool`).
- Wrap domain identifiers (`ClientId`, `CompanionId`, `SubscriptionId`,
  `ConnectionId`) in newtypes in `ene-domain` instead of passing bare `String`.

## Architecture Ratchets

`apps/ene-host/tests/architecture.rs` enforces the rules below and fails the
build when a count rises above its baseline. Never raise a baseline to get a
change through; lower it in the same commit that removes the last violation.
Run it with `cargo test -p ene-host --test architecture`, which `just test` and
CI already cover.

| Rule | Metric | Baseline |
| :--- | :--- | ---: |
| 1 | `ui_rpc_leak` wire refs under `apps/ene-desktop/src/ui/` | 0 |
| 2 | `db_upward_imports` from `apps/ene-host/src/host/db/` | 0 |
| 3 | `impl_file_spread` (inherent `impl` blocks per type across files) | 1 |
| 4 | `providers_no_model_import` | 0 |
| 5 | `host_world_refs` `&World`/`&mut World` in `apps/ene-host` | 159 |
| 6 | `max_prod_file_lines` production files over 500 lines | 2 |
| 7 | `rpc_no_ecs_resource` `#[derive(Resource)]` under `server/rpc/` | 0 |
| 8 | `prod_block_on` `block_on`/`block_in_place` in production | 7 |

Two former rules are now enforced by the compiler, so there is no second copy
to drift: no `println!`/`eprintln!` in production (`clippy::print_stdout`,
`clippy::print_stderr`), and no `unsafe` in `apps/ene-desktop`
(`#![forbid(unsafe_code)]`). A third, `domain_no_rpc_dep`, is a `[[bans.deny]]`
entry in `deny.toml`, which reads the resolved dependency graph.
