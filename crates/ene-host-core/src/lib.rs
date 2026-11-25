//! Host domain logic, kept free of the ECS world, SQLite and the wire protocol.
//!
//! Layer 1 of the workspace layering: pure helpers over L0 types only, so a
//! test can exercise them without a running host. `apps/ene-host` re-exports
//! each module here under its old `host::` path, so moving one in is a change
//! of address with no change at the call site.

// Enforced here first: this crate is free of the lint while the app crates still
// have sites. Add this attribute to a crate only once `cargo clippy -p <crate>`
// reports zero for it. The workspace cannot enable it centrally yet, because a
// crate cannot override `[lints] workspace = true`.
#![warn(clippy::needless_pass_by_value)]
pub mod json;
pub mod prompt;
pub mod util;
