//! Persisted configuration, acceptance receipts and message state transitions.
//! Called on the database thread; no external service is called in a transaction.
use ene_rpc::Message;
use serde_json::{Value, json};
use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub use crate::domain::{Acceptance, Accepted, Configuration};

pub fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC timestamp")
}

mod configuration;
mod messages;
mod receipts;
#[cfg(test)]
mod tests;

pub use configuration::{load, write_configuration};
pub use messages::{accept, update_message};
pub use receipts::{receipt, save_receipt};
