//! The desktop client: the process a person runs.
//!
//! It opens a window, connects to `ene-host`, declares what it can do, and serves
//! the companion the operator is talking to. The avatar renderer is its child,
//! and the audio devices and the window are its own.
//!
//! This is the reference client `docs/design/desktop-client.md` describes. It
//! owns no conversation, no memory and no model: those stay in the host.

mod audio;
mod avatar;
mod capabilities;
mod host;
mod input;
mod perception;
mod session;
mod state;
mod ui;
mod update;

pub use host::{Connect, Discovery, Host};
pub use session::{Connection, Options, Session};
pub use state::{Link, Model};
pub use ui::{Desktop, bind as bind_actions, focus_chat};
pub use update::{Command, Update};
