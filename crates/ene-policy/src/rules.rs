//! Saved policy rules, free-text policies, canonical hashing and gate evaluation.
use crate::capability::{
    COMPUTER_USE, CapabilityApproval, CapabilitySpec, approval_target, truncate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use strum::{EnumString, IntoStaticStr};

use crate::hard_deny::hard_denied_action;
use crate::targets::{ActionTarget, extract_target};

pub(crate) const WEB_SEARCH_TOOL: &str = "web_search";

/// What a saved rule decides when it matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PolicyEffect {
    /// Let the operation run without prompting.
    Allow,
    /// Block the operation.
    Deny,
    /// Require a human approval before the operation runs.
    Confirm,
    /// Ask the model to decide before falling back to a human.
    AskLlm,
}

/// The spelling stored in policy rules.
impl PolicyEffect {
    /// Returns the spelling stored in policy rules.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A saved rule (`AlwaysApprove` saves an exact-invocation allow rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    /// Id of this rule.
    /// Id of this policy text.
    pub id: String,
    /// The tool the approved call would invoke.
    /// Tool or capability the rule applies to; `*` matches any, a trailing `*` is a prefix.
    pub tool_name: String,
    /// `*`, an exact canonical JSON invocation, a scoped digest, or a typed target pattern.
    pub pattern: String,
    /// What the rule decides when it matches.
    pub effect: PolicyEffect,
    /// Creation time, UTC RFC 3339 at whole-second precision.
    pub created_at: String,
}

/// The most characters one free-text policy may carry.
pub const MAX_POLICY_TEXT_CHARS: usize = 500;

/// A free-text policy the decision engine reads (`docs/design/permissions.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyText {
    /// Id of this policy text.
    pub id: String,
    /// The prose itself, at most [`MAX_POLICY_TEXT_CHARS`] characters.
    pub text: String,
    /// Creation time, UTC RFC 3339 at whole-second precision.
    pub created_at: String,
}

/// Lifecycle of an approval request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for an automatic decision. No user-facing card has been issued.
    Evaluating,
    /// Waiting on a person to answer.
    Pending,
    /// A person allowed the operation.
    Approved,
    /// A person refused the operation.
    Denied,
    /// Nobody answered in time, so the operation did not run.
    Expired,
}

impl ApprovalStatus {
    /// Returns the spelling stored in approval rows.
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// An approval request, immutable in the parts that identify the operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// Id of this approval request.
    pub request_id: String,
    /// The agent session that raised the request.
    pub session_id: String,
    /// The run that is waiting on it.
    pub run_id: String,
    /// The work session the run belongs to, if any.
    pub work_id: Option<String>,
    /// SHA-256 of the action payload the person is approving.
    pub action_payload_hash: Vec<u8>,
    /// The tool the approved call would invoke.
    pub tool_name: String,
    /// Canonical JSON arguments, pinned so the approval cannot be widened.
    pub normalized_arguments: String,
    /// Where the request is in its lifecycle.
    pub status: ApprovalStatus,
    /// Creation time, UTC RFC 3339 at whole-second precision.
    pub created_at: String,
    /// When a person answered, if they have.
    pub resolved_at: Option<String>,
    /// The client the action targets, when the action is a device operation.
    pub client_id: Option<String>,
    /// The capability the action invokes, when it names one.
    pub capability: Option<String>,
    /// Display-safe route for an action that sends data to an external service.
    #[serde(default)]
    pub external_destination: Option<String>,
    /// Effective Web search request for review.
    #[serde(default)]
    pub search_summary: Option<SearchApprovalSummary>,
}

/// The display-safe data of a prepared Web search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchApprovalSummary {
    /// The provider that receives the request.
    pub provider: String,
    /// The connection label in Settings.
    pub connection_name: String,
    /// The identifier that distinguishes equally named connections.
    pub connection_id: String,
    /// The query that the provider receives.
    pub query: String,
    /// The result limit that the provider receives.
    pub limit: u32,
}

impl Approval {
    /// Arguments intended for display and review.
    pub fn review_arguments(&self) -> std::borrow::Cow<'_, str> {
        match &self.search_summary {
            Some(search) => serde_json::to_string(search).unwrap_or_default().into(),
            None => self.normalized_arguments.as_str().into(),
        }
    }
}

/// What the Gate should do with a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Runs without prompting.
    Allow,
    /// A person must approve before it runs.
    HumanRequired {
        /// Summary of what the person is approving.
        target: String,
    },
    /// The decision engine must rule before anything runs.
    NeedsDecision {
        /// Summary of what is being decided.
        target: String,
    },
    /// Blocked outright.
    Deny {
        /// Why the action is blocked.
        reason: String,
    },
}

/// Canonical external action evaluated by `Stage::Gate`.
#[derive(Clone, Debug)]
pub enum CandidateAction {
    /// An operation on a client device capability.
    DeviceCapability {
        /// The capability being invoked.
        capability: String,
        /// The client declaration for it, when one is on record.
        spec: Option<Box<CapabilitySpec>>,
        /// The tool that wraps the capability.
        tool_name: String,
        /// The arguments addressed to the capability itself.
        input: Value,
        /// The arguments as the model sent them, before any unwrapping.
        raw_arguments: Value,
    },
    /// A tool call routed to an MCP server.
    McpTool {
        /// Id of the configured MCP connection.
        server_id: String,
        /// The tool the server exposes.
        tool_name: String,
        /// Arguments for the tool.
        input: Value,
    },
    /// A tool call routed to a plugin.
    PluginTool {
        /// Id of the plugin providing the tool.
        plugin_id: String,
        /// The tool the plugin exposes.
        tool_name: String,
        /// Arguments for the tool.
        input: Value,
    },
    /// An outbound web search.
    WebSearch(Arc<WebSearchAction>),
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
    pub raw_arguments: Value,
    /// The host-authenticated identity of the effective invocation.
    pub scope: Option<crate::SearchScope>,
}

/// RFC 8785 (JCS) canonical JSON.
pub fn canonical_json(value: &Value) -> String {
    serde_json_canonicalizer::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// Canonical tool arguments and their SHA-256.
pub fn canonicalise(tool_name: &str, arguments: &Value) -> (String, Vec<u8>) {
    let normalized = canonical_json(arguments);
    let hash = Sha256::digest(format!("{tool_name}\n{normalized}").as_bytes()).to_vec();
    (normalized, hash)
}

/// Lowercase hex of a digest, as stored in rule patterns.
pub fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// An exact Web search rule, scoped to the destination and the arguments the user approved.
fn legacy_search_pattern(endpoint: &str, arguments: &Value) -> String {
    let scoped = serde_json::json!({ "endpoint": endpoint, "arguments": arguments });
    format!(
        "search_sha256:{}",
        hex_digest(&canonicalise(WEB_SEARCH_TOOL, &scoped).1)
    )
}

#[derive(Clone, Copy)]
enum DefaultActionApproval {
    Auto,
    HumanRequired,
    NeedsDecision,
}

struct ResolvedAction<'a> {
    tool_name: &'a str,
    capability: Option<&'a str>,
    server_id: Option<&'a str>,
    raw_args: &'a Value,
    target_summary: String,
    default_approval: DefaultActionApproval,
}

fn resolve_action(action: &CandidateAction) -> ResolvedAction<'_> {
    match action {
        CandidateAction::DeviceCapability {
            capability,
            spec,
            tool_name,
            input,
            raw_arguments,
        } => {
            let approval = match spec.as_deref() {
                Some(spec) if spec.approval == CapabilityApproval::Auto => {
                    DefaultActionApproval::Auto
                }
                _ if capability == COMPUTER_USE => DefaultActionApproval::HumanRequired,
                _ => DefaultActionApproval::NeedsDecision,
            };
            ResolvedAction {
                tool_name: tool_name.as_str(),
                capability: Some(capability.as_str()),
                server_id: None,
                raw_args: raw_arguments,
                target_summary: target(spec.as_deref(), capability, input),
                default_approval: approval,
            }
        }
        CandidateAction::McpTool {
            server_id,
            tool_name,
            input,
        } => ResolvedAction {
            tool_name: tool_name.as_str(),
            capability: None,
            server_id: Some(server_id.as_str()),
            raw_args: input,
            target_summary: format!("{tool_name} {}", truncate(&input.to_string(), 200)),
            default_approval: DefaultActionApproval::NeedsDecision,
        },
        CandidateAction::PluginTool {
            plugin_id,
            tool_name,
            input,
        } => ResolvedAction {
            tool_name: tool_name.as_str(),
            capability: Some(plugin_id.as_str()),
            server_id: None,
            raw_args: input,
            target_summary: format!(
                "plugin:{plugin_id}/{tool_name} {}",
                truncate(&input.to_string(), 180)
            ),
            default_approval: DefaultActionApproval::NeedsDecision,
        },
        CandidateAction::WebSearch(search) => ResolvedAction {
            tool_name: WEB_SEARCH_TOOL,
            capability: None,
            server_id: None,
            raw_args: &search.raw_arguments,
            target_summary: format!(
                "web_search {} ({})",
                search.endpoint.host_str().unwrap_or_default(),
                truncate(&search.query, 80)
            ),
            default_approval: DefaultActionApproval::NeedsDecision,
        },
    }
}

/// Evaluates any `CandidateAction` against Hard Deny, saved `PolicyRule`s, and
/// default approval policy (`docs/design/permissions.md` §3).
pub fn decide_action(rules: &[PolicyRule], action: &CandidateAction) -> Verdict {
    let typed_target = extract_target(action);
    if let Some(reason) = hard_denied_action(action, &typed_target) {
        return Verdict::Deny { reason };
    }
    let resolved = resolve_action(action);
    let (_, raw_hash) = canonicalise(resolved.tool_name, resolved.raw_args);
    let raw_hash_hex = hex_digest(&raw_hash);
    let criteria = ActionMatch {
        tool_name: resolved.tool_name,
        capability: resolved.capability,
        server_id: resolved.server_id,
        raw_arguments: resolved.raw_args,
        raw_hash_hex: &raw_hash_hex,
        typed_target: &typed_target,
        search_scope: match action {
            CandidateAction::WebSearch(search) => search.scope.as_ref(),
            _ => None,
        },
    };
    let mut confirm = false;
    let mut ask_llm = false;
    let mut allowed = false;
    for rule in rules.iter().filter(|rule| matches_action(rule, &criteria)) {
        match rule.effect {
            PolicyEffect::Deny => {
                return Verdict::Deny {
                    reason: format!("saved rule `{}` denies this operation", rule.id),
                };
            }
            PolicyEffect::Confirm => confirm = true,
            PolicyEffect::AskLlm => ask_llm = true,
            PolicyEffect::Allow => allowed = true,
        }
    }
    if confirm {
        return Verdict::HumanRequired {
            target: resolved.target_summary,
        };
    }
    if ask_llm {
        return Verdict::NeedsDecision {
            target: resolved.target_summary,
        };
    }
    if allowed {
        return Verdict::Allow;
    }
    match resolved.default_approval {
        DefaultActionApproval::Auto => Verdict::Allow,
        DefaultActionApproval::HumanRequired => Verdict::HumanRequired {
            target: resolved.target_summary,
        },
        DefaultActionApproval::NeedsDecision => Verdict::NeedsDecision {
            target: resolved.target_summary,
        },
    }
}

/// The decision for a capability operation before the card is shown.
/// Decides a capability operation raised through `device_request`.
pub fn decide_capability(
    rules: &[PolicyRule],
    spec: Option<&CapabilitySpec>,
    capability: &str,
    tool_name: &str,
    call_arguments: &Value,
) -> Verdict {
    let empty = serde_json::json!({});
    let input = if tool_name == "device_request" {
        call_arguments.get("input").unwrap_or(&empty).clone()
    } else {
        call_arguments.clone()
    };
    let action = CandidateAction::DeviceCapability {
        capability: capability.to_owned(),
        spec: spec.cloned().map(Box::new),
        tool_name: tool_name.to_owned(),
        input,
        raw_arguments: call_arguments.clone(),
    };
    decide_action(rules, &action)
}

struct ActionMatch<'a> {
    tool_name: &'a str,
    capability: Option<&'a str>,
    server_id: Option<&'a str>,
    raw_arguments: &'a Value,
    raw_hash_hex: &'a str,
    typed_target: &'a ActionTarget,
    search_scope: Option<&'a crate::SearchScope>,
}

fn matches_action(rule: &PolicyRule, action: &ActionMatch<'_>) -> bool {
    let tool_matches = rule.tool_name == "*"
        || rule.tool_name == action.tool_name
        || action.capability.is_some_and(|cap| rule.tool_name == cap)
        || action
            .server_id
            .is_some_and(|srv| rule.tool_name == format!("mcp:{srv}"))
        || rule.tool_name.strip_suffix('*').is_some_and(|prefix| {
            !prefix.is_empty()
                && (action.tool_name.starts_with(prefix)
                    || action.capability.is_some_and(|cap| cap.starts_with(prefix)))
        });
    if !tool_matches {
        return false;
    }
    if rule.pattern == "*" {
        return true;
    }
    if let Some((tag, _)) = rule.search_scope() {
        return action.tool_name == WEB_SEARCH_TOOL
            && action.search_scope.is_some_and(|scope| scope.matches(tag));
    }
    // Legacy exact search allows did not bind the effective route and account.
    let argument_allow_is_unscoped =
        action.tool_name == WEB_SEARCH_TOOL && rule.effect == PolicyEffect::Allow;
    if let Some(expected_hex) = rule.pattern.strip_prefix("exact_sha256:") {
        return !argument_allow_is_unscoped && expected_hex == action.raw_hash_hex;
    }
    if rule.pattern.starts_with("search_sha256:") {
        return !argument_allow_is_unscoped
            && action.tool_name == WEB_SEARCH_TOOL
            && matches!(action.typed_target, ActionTarget::Url { url, .. }
                if rule.pattern == legacy_search_pattern(url, action.raw_arguments));
    }
    if rule.pattern.starts_with('{') {
        return !argument_allow_is_unscoped
            && serde_json::from_str::<Value>(&rule.pattern)
                .ok()
                .is_some_and(|saved| {
                    canonical_json(&saved) == canonical_json(action.raw_arguments)
                });
    }
    action.typed_target.matches_pattern(&rule.pattern)
}

fn target(spec: Option<&CapabilitySpec>, capability: &str, arguments: &Value) -> String {
    match spec {
        Some(spec) => approval_target(spec, arguments),
        None => format!("{capability} {}", truncate(&arguments.to_string(), 200)),
    }
}

/// What a person answered on an approval card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// No answer arrived.
    #[default]
    Unspecified,
    /// Allow this one call.
    Approve,
    /// Persists an exact-invocation allow rule for future matching calls.
    AlwaysApprove,
    /// Refuse this call.
    Deny,
}
