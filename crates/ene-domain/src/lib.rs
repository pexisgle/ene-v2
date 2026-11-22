//! Pure domain data types shared across persistence and host modules.
pub mod client;
pub mod failure;
pub mod work;

pub use client::{Client, DeviceOperation, DeviceStatus};
pub use failure::Failure;
pub use work::{Run, RunState, WorkSession, WorkStatus};
