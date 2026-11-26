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
([`docs/architecture.md`](docs/architecture.md)):

| Layer | Crates | May depend on | Never |
| :--- | :--- | :--- | :--- |
| **L0 Pure domain** | `ene-domain`, `ene-policy`, `ene-catalog`, `ene-mind`, `ene-work`, `ene-voice`, `ene-plugin` | std, serialization helpers, lower L0 crates | `ene-rpc`, `rusqlite`, `reqwest`, `axum`, `gpui`, `bevy_ecs`, OS FFI |
| **L1 Application** | `ene-client-core` | L0 only; external I/O through port traits | concrete DB/HTTP/RPC/Win32 implementations |
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
| 3 | `impl_file_spread` (inherent `impl` blocks per type across files) | 0 |
| 4 | `providers_no_model_import` | 0 |
| 5 | `host_world_refs` `&World`/`&mut World` in `apps/ene-host` | 125 |
| 6 | `max_prod_file_lines` production files over 500 lines | 0 |
| 7 | `rpc_no_ecs_resource` `#[derive(Resource)]` under `server/rpc/` | 0 |
| 8 | `prod_block_on` `block_on`/`block_in_place` in production | 0 |

`needless_pass_by_value` is enforced per crate, not workspace-wide: every crate
except `ene-vrm` and the three apps carries
`#![warn(clippy::needless_pass_by_value)]`, so the CI clippy step fails if one of
those regresses.

`ene-vrm` (4 sites) and `apps/ene-avatar` (14) cannot carry it. Thirteen of avatar's are Bevy `SystemParam` types (`Res<_>`, `On<_>`, `ExWlShellEvent`) which have no reference form, as are three of vrm's; the remainder in both is `HipsRetarget`, whose borrow escapes into a stored closure. Only `apps/ene-host` (55) and `apps/ene-desktop` (50) have sites that can be fixed.

A crate cannot add the lint to `[lints]` while `workspace = true` — cargo rejects the override — so the inner attribute is the only per-crate form. Add it to one of those four only once `cargo clippy -p <crate>` reports zero for it, never as an `#[allow]`.

## GPUI components

`ene-desktop` uses GPUI Kit through the workspace's `gpui` dependency alias.
The Kit release pins the matching GPUI crates. Initialize its components before
opening windows with `gpui::open_window`, which installs the component root.
Use the desktop palette in `apps/ene-desktop/src/ui/theme.rs`. Verify changes to
text entry and selection with the GUI test harness and check native IME behavior
on the affected platforms.

## OS perception on Linux

`apps/ene-desktop/src/perception/platform_linux.rs` uses `zwlr_foreign_toplevel`
directly, so it observes the active window on **any** wlroots compositor. Do not
replace it with `active-win-pos-rs`: that crate's Wayland support is
`try_kwin().or_else(try_hyprland)` and returns nothing anywhere else (verified on
a non-KWin, non-Hyprland session). It also would not reduce `unsafe`, because
this file contains none — the workspace's `unsafe` is Win32 FFI inside
`ene-platform-os`.

Two former rules are now enforced by the compiler, so there is no second copy
to drift: no `println!`/`eprintln!` in production (`clippy::print_stdout`,
`clippy::print_stderr`), and no `unsafe` in `apps/ene-desktop`
(`#![forbid(unsafe_code)]`). A third, `domain_no_rpc_dep`, is a `[[bans.deny]]`
entry in `deny.toml`, which reads the resolved dependency graph.
