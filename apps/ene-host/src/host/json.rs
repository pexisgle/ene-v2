//! JSON documents on the wire and the schema validation the host performs.
//!
//! Capability inputs, capability outputs and `device_request` arguments are
//! validated via `ene_domain::json` using `jsonschema` against the schema the
//! client declared, so the standard keywords (`pattern`, `anyOf`, ...) behave
//! as specified instead of a hand-written subset.

use super::error::Result;
use serde_json::Value;

pub(crate) use ene_domain::json::validate;
pub(crate) use ene_mind::JsonDoc;

/// Parses a JSON Schema document, which must be an object.
pub(crate) fn parse_schema(text: &str) -> Result<Value> {
    ene_domain::json::parse_schema(text).map_err(Into::into)
}
