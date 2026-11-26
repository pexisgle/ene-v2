//! The host-side grant policy for declared client capabilities.

use crate::host::permission;
use bevy_ecs::prelude::*;

/// The environment variable that decides which declared capabilities the host
/// grants: a comma list of names, or `*` for every declaration. It is read
/// once when the app is built; an unset or empty value grants nothing.
pub(crate) const ALLOWED_CAPABILITIES_ENV: &str = "ENE_HOST_ALLOWED_CAPABILITIES";

/// What the host is willing to grant a client. A declaration alone grants
/// nothing: the policy is the host-side decision, and the operation-level
/// gate still decides every call.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapabilityPolicy {
    pub(crate) allowed: Vec<String>,
    /// `*` in the environment: grant whatever a client declares.
    pub(crate) all: bool,
}

impl CapabilityPolicy {
    /// Nothing is granted: the default when the environment names no policy.
    pub(crate) fn none() -> Self {
        Self {
            allowed: Vec::new(),
            all: false,
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut policy = Self::none();
        for capability in text
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            if capability == "*" {
                policy.all = true;
            } else if !policy.allowed.iter().any(|allowed| allowed == capability) {
                policy.allowed.push(capability.to_owned());
            }
        }
        policy
    }

    pub(crate) fn allows(&self, capability: &str) -> bool {
        self.all
            || self.allowed.iter().any(|allowed| {
                allowed == capability
                    || allowed
                        .strip_suffix('*')
                        .is_some_and(|prefix| !prefix.is_empty() && capability.starts_with(prefix))
            })
    }

    /// Dynamically allows a capability pattern (`exact` or `prefix.*`) at runtime (`F11`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn allow_pattern(&mut self, pattern: impl Into<String>) {
        let pattern = pattern.into();
        let trimmed = pattern.trim();
        if trimmed == "*" {
            self.all = true;
        } else if !trimmed.is_empty() && !self.allowed.iter().any(|p| p == trimmed) {
            self.allowed.push(trimmed.to_owned());
        }
    }

    /// Dynamically revokes a capability pattern at runtime (`F11`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn revoke_pattern(&mut self, pattern: &str) {
        let trimmed = pattern.trim();
        if trimmed == "*" {
            self.all = false;
        } else {
            self.allowed.retain(|p| p != trimmed);
        }
    }

    /// The declared capabilities this policy grants, in declaration order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn grants(&self, declared: &[String]) -> Vec<String> {
        declared
            .iter()
            .filter(|capability| self.allows(capability))
            .cloned()
            .collect()
    }

    /// Grants declared capabilities allowed by either the static env policy or
    /// saved `PolicyRule`s targeting `client_capability` (`F11`).
    pub(crate) fn grants_with_rules(
        &self,
        declared: &[String],
        rules: &[permission::PolicyRule],
    ) -> Vec<String> {
        use permission::PolicyEffect;
        declared
            .iter()
            .filter(|capability| {
                let mut denied = false;
                let mut rule_allowed = false;
                for rule in rules
                    .iter()
                    .filter(|r| r.tool_name == "client_capability" || r.tool_name == "*")
                {
                    let matches = rule.pattern == "*"
                        || rule.pattern == **capability
                        || rule.pattern.strip_suffix('*').is_some_and(|prefix| {
                            !prefix.is_empty() && capability.starts_with(prefix)
                        });
                    if !matches {
                        continue;
                    }
                    match rule.effect {
                        PolicyEffect::Deny => {
                            denied = true;
                            break;
                        }
                        PolicyEffect::Allow => rule_allowed = true,
                        PolicyEffect::Confirm | PolicyEffect::AskLlm => {}
                    }
                }
                !denied && (self.allows(capability) || rule_allowed)
            })
            .cloned()
            .collect()
    }
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        std::env::var(ALLOWED_CAPABILITIES_ENV)
            .map_or_else(|_| Self::none(), |text| Self::parse(&text))
    }
}
