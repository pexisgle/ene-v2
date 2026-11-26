//! Branded domain identifiers.
//!
//! The crates pass identifiers around as `String`, so a companion id and a
//! client id are interchangeable to the compiler. A newtype fixes that at the
//! boundary where the id is parsed, and costs nothing at runtime.

use crate::error::{DomainError, validate_id};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Borrow, fmt, sync::Arc};

/// The id of one companion.
///
/// Construction is the only validation point: it refuses anything the domain
/// rules reject, so a value of this type is always a valid id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompanionId(String);

impl CompanionId {
    /// Parses and validates a companion id.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] when the id is empty, longer than 128
    /// characters, or contains anything but printable ASCII.
    pub fn parse(id: impl Into<String>) -> Result<Self, DomainError> {
        let id = id.into();
        validate_id(&id)?;
        Ok(Self(id))
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CompanionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for CompanionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for CompanionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompanionId {
    /// Deserialises through `String` and validates, so a wire payload cannot
    /// carry an id the domain would refuse.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        validate_id(&id).map_err(serde::de::Error::custom)?;
        Ok(Self(id))
    }
}

/// The validated id of a client, shared by live device and audio streams.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClientId(Arc<str>);

impl ClientId {
    /// Parses a client id using the domain's printable ASCII rules.
    ///
    /// # Errors
    /// Returns [`DomainError::Invalid`] for an invalid id.
    pub fn parse(id: &str) -> Result<Self, DomainError> {
        validate_id(id)?;
        Ok(Self(Arc::from(id)))
    }

    /// The id as a string slice for storage and protocol adapters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for ClientId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// The generation of one live device stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(uuid::Uuid);

impl From<uuid::Uuid> for SubscriptionId {
    fn from(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

impl SubscriptionId {
    /// Checks the canonical spelling published when the stream opened.
    pub fn matches_wire(self, text: &str) -> bool {
        self.0
            .hyphenated()
            .encode_lower(&mut uuid::Uuid::encode_buffer())
            == text
    }
}

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_round_trips_through_json() {
        let id = CompanionId::parse("ene").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"ene\"");
        assert_eq!(serde_json::from_str::<CompanionId>(&json).unwrap(), id);
    }

    #[test]
    fn a_deserialised_id_is_validated() {
        let refused = serde_json::from_str::<CompanionId>("\"bad id with spaces\"");
        assert!(refused.is_err(), "a space is not printable-id ASCII");
        let empty = serde_json::from_str::<CompanionId>("\"\"");
        assert!(empty.is_err(), "an empty id is refused");
    }

    #[test]
    fn parse_refuses_what_the_domain_refuses() {
        assert!(CompanionId::parse("").is_err());
        assert!(CompanionId::parse("a".repeat(129)).is_err());
        assert!(CompanionId::parse("ene").is_ok());
    }
}

#[cfg(feature = "sqlite")]
mod sqlite {
    use super::CompanionId;
    use rusqlite::{
        Result as SqlResult, ToSql, types::FromSql, types::FromSqlResult, types::ToSqlOutput,
        types::ValueRef,
    };

    impl ToSql for CompanionId {
        fn to_sql(&self) -> SqlResult<ToSqlOutput<'_>> {
            self.as_str().to_sql()
        }
    }

    impl FromSql for CompanionId {
        /// Reads the stored string and re-validates it, so a hand-edited row
        /// cannot introduce an id the domain would refuse.
        fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
            let id = String::column_result(value)?;
            CompanionId::parse(id)
                .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
        }
    }
}
