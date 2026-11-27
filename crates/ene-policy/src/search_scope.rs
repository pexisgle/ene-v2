use crate::{PolicyEffect, PolicyRule, hex_digest};

const PREFIX: &str = "search_hmac_v1:";

pub(super) fn pattern_parts(pattern: &str) -> Option<(&str, &str)> {
    pattern.strip_prefix(PREFIX)?.split_once('\n')
}

impl PolicyRule {
    /// The reviewable label of a saved Web search, when present.
    pub fn search_label(&self) -> Option<&str> {
        let (_, label) = pattern_parts(&self.pattern)?;
        Some(label)
    }
}

/// An authenticated effective search scope with a presentation-only label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchScope {
    tag: String,
    label: String,
}

impl SearchScope {
    /// Binds the host-issued authentication tag to a safe label for settings.
    pub fn new(tag: [u8; 32], label: String) -> Self {
        Self {
            tag: hex_digest(&tag),
            label,
        }
    }

    pub(super) fn matches(&self, tag: &str) -> bool {
        self.tag == tag
    }

    /// Saves one rule per effective invocation, including across host restarts.
    pub fn allow_rule(&self) -> PolicyRule {
        PolicyRule {
            id: format!("web-search-{}", self.tag),
            tool_name: crate::rules::WEB_SEARCH_TOOL.into(),
            pattern: format!("{PREFIX}{}\n{}", self.tag, self.label),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        }
    }
}
