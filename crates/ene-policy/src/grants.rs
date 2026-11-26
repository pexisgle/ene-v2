//! Grants for declared capabilities, separate from approval of individual operations.

use crate::{PolicyEffect, PolicyRule};

/// The host's startup allow list. Saved policy rules can grant or deny
/// additional capabilities. A matching deny takes precedence.
#[derive(Clone, Debug, Default)]
pub struct CapabilityGrants(Vec<String>);

impl CapabilityGrants {
    /// Parses the startup list of comma-separated exact names or suffix wildcards.
    pub fn parse(list: &str) -> Self {
        Self(
            list.split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }

    /// Projects the current grants in declaration order.
    pub fn granted(&self, declared: &[String], rules: &[PolicyRule]) -> Vec<String> {
        declared
            .iter()
            .filter(|capability| {
                let mut allowed = self.0.iter().any(|pattern| matches(pattern, capability));
                for rule in rules.iter().filter(|rule| {
                    // Global operation rules cannot grant a capability. Keep
                    // historical wildcard denials effective for permissions
                    // such as perception that do not enter the operation gate.
                    (rule.tool_name == "client_capability"
                        || (rule.tool_name == "*" && rule.effect == PolicyEffect::Deny))
                        && matches(&rule.pattern, capability)
                }) {
                    match rule.effect {
                        PolicyEffect::Deny => return false,
                        PolicyEffect::Allow => allowed = true,
                        PolicyEffect::Confirm | PolicyEffect::AskLlm => {}
                    }
                }
                allowed
            })
            .cloned()
            .collect()
    }
}

fn matches(pattern: &str, name: &str) -> bool {
    pattern == name
        || pattern
            .strip_suffix('*')
            .is_some_and(|prefix| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_rules_extend_startup_grants_and_denial_wins() {
        let startup = CapabilityGrants::parse(" body.motion, plugin.echo.* ");
        let declared = [
            "body.motion".into(),
            "plugin.echo.run".into(),
            "plugin.other.run".into(),
            "plugin.skip.run".into(),
        ];
        let rule = |id: &str, pattern: &str, effect| PolicyRule {
            id: id.into(),
            tool_name: "client_capability".into(),
            pattern: pattern.into(),
            effect,
            created_at: String::new(),
        };
        assert_eq!(
            startup.granted(&declared, &[]),
            ["body.motion", "plugin.echo.run"]
        );
        let rules = [
            rule("deny", "plugin.skip.*", PolicyEffect::Deny),
            rule("allow", "plugin.*", PolicyEffect::Allow),
        ];
        assert_eq!(
            startup.granted(&declared, &rules),
            ["body.motion", "plugin.echo.run", "plugin.other.run"]
        );
        let operation_rule = PolicyRule {
            tool_name: "*".into(),
            ..rule("skip-operations", "*", PolicyEffect::Allow)
        };
        assert_eq!(
            startup.granted(&declared, &[operation_rule]),
            ["body.motion", "plugin.echo.run"]
        );
    }
}
