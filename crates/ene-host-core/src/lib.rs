//! Host domain logic, kept free of the ECS world, SQLite and the wire protocol.
//!
//! Layer 1 of the workspace layering: pure helpers over L0 types only, so a
//! test can exercise them without a running host. `apps/ene-host` re-exports
//! each module here under its old `host::` path, so moving one in is a change
//! of address with no change at the call site.
pub mod util;
