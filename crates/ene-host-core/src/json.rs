//! JSON documents on the wire and the schema validation the host performs.
//!
//! Capability inputs, capability outputs and `device_request` arguments are
//! validated via `ene_domain::json` using `jsonschema` against the schema the
//! client declared, so the standard keywords (`pattern`, `anyOf`, ...) behave
//! as specified instead of a hand-written subset.
//!
//! Returns [`DomainError`] rather than the host's error type: this is layer 1,
//! and `HostError` carries a `rusqlite` variant that layer 1 must not reach.
//! `apps/ene-host` converts with the `From<DomainError>` impl it already has.

use ene_domain::DomainError;
use serde_json::Value;

pub use ene_domain::json::validate;
pub use ene_mind::JsonDoc;

/// Parses a JSON Schema document, which must be an object.
pub fn parse_schema(text: &str) -> Result<Value, DomainError> {
    ene_domain::json::parse_schema(text)
}
