//! Branded domain identifiers.
//!
//! Parsed identifiers keep companion, client and connection references distinct.

use crate::error::{DomainError, validate_id};
use serde::{Deserialize, Serialize};
use std::{borrow::Borrow, fmt, sync::Arc};

macro_rules! validated_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            /// Parses an ID of 1–128 printable ASCII characters.
            ///
            /// # Errors
            /// Returns a domain error if the ID is invalid.
            pub fn parse(id: impl Into<String>) -> Result<Self, DomainError> {
                Self::try_from(id.into())
            }

            /// Returns the ID as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = DomainError;

            fn try_from(id: String) -> Result<Self, Self::Error> {
                validate_id(&id)?;
                Ok(Self(id))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }
    };
}

validated_id!(CompanionId, "The validated ID of a companion.");
validated_id!(ConnectionId, "The validated ID of a host connection.");

impl From<ConnectionId> for String {
    fn from(id: ConnectionId) -> Self {
        id.0
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
        assert_eq!(
            serde_json::from_str::<ConnectionId>(&json)
                .unwrap()
                .as_str(),
            "ene"
        );
    }

    #[test]
    fn a_deserialised_id_is_validated() {
        let refused = serde_json::from_str::<CompanionId>("\"bad id with spaces\"");
        assert!(refused.is_err(), "a space is not printable-id ASCII");
        let empty = serde_json::from_str::<CompanionId>("\"\"");
        assert!(empty.is_err(), "an empty id is refused");
        assert!(serde_json::from_str::<ConnectionId>("\"bad id\"").is_err());
    }

    #[test]
    fn parse_refuses_what_the_domain_refuses() {
        assert!(CompanionId::parse("").is_err());
        assert!(CompanionId::parse("a".repeat(129)).is_err());
        assert!(CompanionId::parse("ene").is_ok());
    }
}
