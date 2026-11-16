//! The desktop client: the process a person runs.
//!
//! It connects to `ene-host`, declares what it can do, becomes the active
//! device for a companion, and serves that companion's device operations while
//! it runs. The avatar renderer is its child, and the audio devices are its own.
//!
//! This is the reference client `docs/design/desktop-client.md` describes. It
//! owns no conversation, no memory and no model: those stay in the host.

mod audio;
mod avatar;
mod capabilities;
mod client;
mod host;
mod perception;

pub use client::{Client, Options};
pub use host::{Connect, Discovery, Host};
