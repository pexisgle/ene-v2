//! Persisted configuration, acceptance receipts and message state transitions.
//! Called on the database thread; no external service is called in a transaction.
use serde_json::{Value, json};
use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, params};

pub use crate::domain::Configuration;

mod configuration;
mod receipts;
#[cfg(test)]
mod tests;

pub use configuration::{load, write_configuration};
pub(crate) use receipts::request_id;
pub use receipts::{complete_receipt, receipt, release_receipt, save_receipt};
