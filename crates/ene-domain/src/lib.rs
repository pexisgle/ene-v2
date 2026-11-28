//! Identifiers shared by the host and its clients.

pub mod agent_platform;

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;

#[derive(Debug, thiserror::Error)]
#[error("invalid identifier")]
pub struct InvalidId;

pub fn validate_id(value: &str) -> Result<(), InvalidId> {
    if (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(InvalidId)
    }
}

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                validate_id(&value)?;
                Ok(Self(value))
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                self.as_str()
            }
        }
    };
}

identifier!(ClientId);
identifier!(CompanionId);
identifier!(ConnectionId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionId(std::num::NonZeroU64);

impl SubscriptionId {
    pub fn from_generation(generation: u64) -> Option<Self> {
        std::num::NonZeroU64::new(generation).map(Self)
    }
}
