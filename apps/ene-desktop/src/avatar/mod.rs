//! The avatar renderer as a child process.
//!
//! `apps/ene-avatar` speaks JSON Lines on stdin and stdout (`src/control.rs`).
//! This module owns that process: it frames requests, drains replies and
//! notifications, and turns a `body.motion` operation into an `add` or a `set`.
//!
//! `set` replaces the whole character configuration, so this module keeps the
//! configuration it last sent and resends it with the one field that changed.
//! Sending only a motion would silently reset the monitor, position and size.
//!
//! The renderer has no network and no host connection. It knows a character id,
//! a model path and a motion path; everything else is this client's job.

mod client;
mod presets;
#[cfg(test)]
mod tests;

pub use client::{Avatar, AvatarError};
pub use presets::{
    MOTIONS, bundled_ene_model, default_model, default_renderer, motion, resolve_model_path,
};
