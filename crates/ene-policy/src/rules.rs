//! Saved policy rules, free-text policies, canonical hashing and gate evaluation.
use crate::capability::{
    COMPUTER_USE, CapabilityApproval, CapabilitySpec, approval_target, truncate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use strum::{EnumString, IntoStaticStr};

use crate::hard_deny::hard_denied_action;
use crate::targets::{ActionTarget, extract_target};

pub(crate) const WEB_SEARCH_TOOL: &str = "web_search";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Deny,
    Confirm,
    AskLlm,
}

/// The spelling stored in policy rules.
impl PolicyEffect {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// A saved rule (`AlwaysApprove` saves an exact-invocation allow rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRule {
    pub id: String,
    pub tool_name: String,
    /// `*`, an exact canonical JSON invocation, `exact_sha256:<hex>`, or a typed target pattern.
    pub pattern: String,
    pub effect: PolicyEffect,
    pub created_at: String,
}

/// The most characters one free-text policy may carry.
pub const MAX_POLICY_TEXT_CHARS: usize = 500;

/// A free-text policy the decision engine reads (`docs/design/permissions.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyText {
    pub id: String,
    pub text: String,
    pub created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, EnumString, IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// An approval request, immutable in the parts that identify the operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub request_id: String,
    pub session_id: String,
    pub run_id: String,
    pub work_id: Option<String>,
    pub action_payload_hash: Vec<u8>,
    pub tool_name: String,
    pub normalized_arguments: String,
    pub status: ApprovalStatus,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub client_id: Option<String>,
    pub capability: Option<String>,
}

/// What the Gate should do with a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    HumanRequired { target: String },
    NeedsDecision { target: String },
    Deny { reason: String },
}

/// Canonical external action evaluated by `Stage::Gate`.
#[derive(Clone, Debug)]
pub enum CandidateAction {
    DeviceCapability {
        capability: String,
        spec: Option<Box<CapabilitySpec>>,
        tool_name: String,
        input: Value,
        raw_arguments: Value,
    },
    McpTool {
        server_id: String,
        tool_name: String,
        input: Value,
    },
    PluginTool {
        plugin_id: String,
        tool_name: String,
        input: Value,
    },
    WebSearch {
        endpoint: String,
        query: String,
        input: Value,
    },
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

pub fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(bytes)
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

fn resolve_action<'a>(
    action: &'a CandidateAction,
    typed_target: &'a ActionTarget,
) -> ResolvedAction<'a> {
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
        CandidateAction::WebSearch {
            endpoint,
            query,
            input,
        } => {
            let host = match typed_target {
                ActionTarget::Url { host: Some(h), .. } => h.as_str(),
                _ => endpoint.as_str(),
            };
            ResolvedAction {
                tool_name: WEB_SEARCH_TOOL,
                capability: None,
                server_id: None,
                raw_args: input,
                target_summary: format!("web_search {host} ({})", truncate(query, 80)),
                default_approval: DefaultActionApproval::NeedsDecision,
            }
        }
    }
}

/// Evaluates any `CandidateAction` against Hard Deny, saved `PolicyRule`s, and
/// default approval policy (`docs/design/permissions.md` §3).
pub fn decide_action(rules: &[PolicyRule], action: &CandidateAction) -> Verdict {
    let typed_target = extract_target(action);
    if let Some(reason) = hard_denied_action(action, &typed_target) {
        return Verdict::Deny { reason };
    }
    let resolved = resolve_action(action, &typed_target);
    let (_, raw_hash) = canonicalise(resolved.tool_name, resolved.raw_args);
    let raw_hash_hex = hex_digest(&raw_hash);
    let criteria = ActionMatch {
        tool_name: resolved.tool_name,
        capability: resolved.capability,
        server_id: resolved.server_id,
        raw_arguments: resolved.raw_args,
        raw_hash_hex: &raw_hash_hex,
        typed_target: &typed_target,
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
    if let Some(expected_hex) = rule.pattern.strip_prefix("exact_sha256:") {
        return expected_hex == action.raw_hash_hex;
    }
    if rule.pattern.starts_with('{') {
        return serde_json::from_str::<Value>(&rule.pattern)
            .ok()
            .is_some_and(|saved| canonical_json(&saved) == canonical_json(action.raw_arguments));
    }
    action.typed_target.matches_pattern(&rule.pattern)
}

fn target(spec: Option<&CapabilitySpec>, capability: &str, arguments: &Value) -> String {
    match spec {
        Some(spec) => approval_target(spec, arguments),
        None => format!("{capability} {}", truncate(&arguments.to_string(), 200)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Unspecified,
    Approve,
    /// Persists an exact-invocation allow rule for future matching calls.
    AlwaysApprove,
    Deny,
}
