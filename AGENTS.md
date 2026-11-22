Read README.md carefully and follow the development instructions provided there accordingly.

## Structural & Code Quality Rules

- Keep every `.rs` file concise (target <= 500 lines, hard limit < 1,000 lines) and every function <= 120 lines (cognitive complexity <= 20, arguments <= 6). Split by responsibility when approaching limits.
- Do not place Protobuf conversions (`w::*` mutations), vendor/adapter branching, or RPC orchestration inside `apps/ene-desktop/src/ui/`.
- Do not introduce `Result<_, String>` in new code; define structured domain errors with `thiserror`.
- Any `#[allow(clippy::...)]` attribute must include an immediately preceding comment explaining why it is required and the condition for removing it.
- Keep crate dependencies strictly acyclic and unidirectional:
  - `apps/*` (`ene-host`, `ene-desktop`, `ene-avatar`)
  - -> Layer 2 (`ene-client-core`, `ene-plugin-runtime`, `ene-store`, `ene-vrm`)
  - -> Layer 1 (`ene-domain`, `ene-policy`, `ene-mind`, `ene-catalog`, `ene-rpc`)
  - -> Layer 0 (`ene-plugin`, `ene-avatar-ipc`)
