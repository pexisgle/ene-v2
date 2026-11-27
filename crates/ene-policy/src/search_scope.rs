use crate::{PolicyEffect, PolicyRule, hex_digest};

const LEGACY_PREFIX: &str = "search_hmac_v1:";
const PREFIX: &str = "search_scope_v2\n";
const ID_PREFIX: &str = "web-search-";

impl PolicyRule {
    pub(super) fn search_scope(&self) -> Option<(&str, &str)> {
        if let Some(label) = self.pattern.strip_prefix(PREFIX) {
            return Some((self.id.strip_prefix(ID_PREFIX)?, label));
        }
        self.pattern.strip_prefix(LEGACY_PREFIX)?.split_once('\n')
    }

    /// The reviewable label of a saved Web search, when present.
    pub fn search_label(&self) -> Option<&str> {
        self.search_scope().map(|(_, label)| label)
    }

    /// The public identifier of the exact saved search scope.
    pub fn search_reference(&self) -> Option<&str> {
        self.search_scope().map(|(tag, _)| tag)
    }

    /// Legacy literal Allow rules cannot authorize a Web search.
    pub fn is_inactive_search_allow(&self) -> bool {
        self.tool_name == crate::rules::WEB_SEARCH_TOOL
            && self.effect == PolicyEffect::Allow
            && (self.pattern.starts_with("exact_sha256:")
                || self.pattern.starts_with("search_sha256:")
                || self.pattern.starts_with('{'))
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
            id: format!("{ID_PREFIX}{}", self.tag),
            tool_name: crate::rules::WEB_SEARCH_TOOL.into(),
            pattern: format!("{PREFIX}{}", self.label),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        }
    }
}
