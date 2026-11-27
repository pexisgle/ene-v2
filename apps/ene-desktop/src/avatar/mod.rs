//! The avatar renderer as a child process.
//!
//! `apps/ene-avatar` speaks JSON Lines on stdin and stdout (`src/control.rs`).
//! This module owns that process: it frames requests, drains replies and
//! notifications, and dispatches body operations to the displayed model.
//! Binding changes reconcile model and display settings separately from motions.
//!
//! The renderer has no network and no host connection. It knows a character id,
//! a model path and a motion path; everything else is this client's job.

mod character;
mod client;
mod presets;
#[cfg(test)]
mod tests;

pub use client::{Avatar, AvatarError};
pub use presets::{
    MOTIONS, bundled_ene_model, default_model, default_renderer, motion, resolve_model_path,
};
