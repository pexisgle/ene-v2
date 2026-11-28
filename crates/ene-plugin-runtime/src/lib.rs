//! Bounded Wasm plugin execution shared by the host and desktop.

#![cfg(not(target_family = "wasm"))]

mod runtime;

pub use runtime::{Package, instance, limits};
