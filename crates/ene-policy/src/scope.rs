use crate::{PolicyEffect, PolicyRule, hex_digest};

const LEGACY_SEARCH: &str = "search_hmac_v1:";
const SEARCH_PATTERN: &str = "search_scope_v2\n";
const SEARCH_ID: &str = "web-search-";
const TOOL_PATTERN: &str = "tool_scope_v1\n";
const TOOL_ID: &str = "external-tool-";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScopeKind {
    Search,
    Tool,
}

impl PolicyRule {
    pub(super) fn scope_parts(&self) -> Option<(ScopeKind, &str, &str)> {
        for (prefix, id, kind) in [
            (SEARCH_PATTERN, SEARCH_ID, ScopeKind::Search),
            (TOOL_PATTERN, TOOL_ID, ScopeKind::Tool),
        ] {
            if let Some(label) = self.pattern.strip_prefix(prefix) {
                return Some((kind, self.id.strip_prefix(id)?, label));
            }
        }
        let (tag, label) = self.pattern.strip_prefix(LEGACY_SEARCH)?.split_once('\n')?;
        Some((ScopeKind::Search, tag, label))
    }

    /// The reviewable label for a scoped external operation, when present.
    pub fn scope_label(&self) -> Option<&str> {
        self.scope_parts().map(|(_, _, label)| label)
    }

    /// The public identifier of a saved operation's exact scope.
    pub fn scope_reference(&self) -> Option<&str> {
        self.scope_parts().map(|(_, tag, _)| tag)
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

/// An authenticated operation scope with a presentation-only label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvocationScope {
    kind: ScopeKind,
    tool: String,
    tag: String,
    label: String,
}

impl InvocationScope {
    /// An effective Web search.
    pub fn search(tag: [u8; 32], label: String) -> Self {
        Self {
            kind: ScopeKind::Search,
            tool: crate::rules::WEB_SEARCH_TOOL.into(),
            tag: hex_digest(&tag),
            label,
        }
    }

    /// An invocation of a tool at its verified installation.
    pub fn tool(tool: String, tag: [u8; 32], label: String) -> Self {
        Self {
            kind: ScopeKind::Tool,
            tool,
            tag: hex_digest(&tag),
            label,
        }
    }

    pub(super) fn matches(&self, kind: ScopeKind, tag: &str) -> bool {
        self.kind == kind && self.tag == tag
    }

    /// Saves one rule per effective invocation, including across host restarts.
    pub fn allow_rule(&self) -> PolicyRule {
        let (id, pattern) = match self.kind {
            ScopeKind::Search => (SEARCH_ID, SEARCH_PATTERN),
            ScopeKind::Tool => (TOOL_ID, TOOL_PATTERN),
        };
        PolicyRule {
            id: format!("{id}{}", self.tag),
            tool_name: self.tool.clone(),
            pattern: format!("{pattern}{}", self.label),
            effect: PolicyEffect::Allow,
            created_at: String::new(),
        }
    }
}

/// A tool invocation bound to one installed plugin.
#[derive(Clone, Debug)]
pub struct PluginToolAction {
    /// Id of the plugin providing the tool.
    pub plugin_id: String,
    /// The tool the plugin exposes.
    pub tool_name: String,
    /// The tool arguments.
    pub input: serde_json::Value,
    /// The host-authenticated identity of the installed invocation.
    pub scope: Option<crate::InvocationScope>,
}

/// The effective query and destination, with literal arguments for older rules.
#[derive(Clone, Debug)]
pub struct WebSearchAction {
    /// The validated endpoint.
    pub endpoint: url::Url,
    /// The query as sent.
    pub query: String,
    /// The number of results requested after applying the connection cap.
    pub limit: usize,
    /// Original invocation for legacy exact rules and the approval hash.
    pub raw_arguments: serde_json::Value,
    /// The host-authenticated identity of the effective invocation.
    pub scope: Option<crate::InvocationScope>,
}
