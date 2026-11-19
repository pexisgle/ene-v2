//! Approval requests, saved policy rules, free-text policies and the
//! decision the Gate applies.
//!
//! The pipeline follows docs/design/permissions.md §3: hard denial, explicit
//! every-time confirmation, saved rules, then the Decision API, then the
//! user. A call the rules do not decide is held with a pending approval and
//! judged by the decision model in the background; the card appears only
//! when the model requires approval or the call fails (fail-safe).
use super::client::{BUILTIN_TOOLS, CapabilityApproval, CapabilitySpec};
use super::db::{digest, truncate};
use super::error::{Failure, HostError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PolicyEffect {
    Allow,
    Deny,
    Confirm,
}

/// The spelling stored in policy rules.
impl PolicyEffect {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Confirm => "confirm",
        }
    }
}

/// A saved rule (`AlwaysApprove` saves an exact-invocation allow rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyRule {
    pub(crate) id: String,
    pub(crate) tool_name: String,
    /// `*`, an exact canonical JSON invocation, `exact_sha256:<hex>`, or a typed target pattern.
    pub(crate) pattern: String,
    pub(crate) effect: PolicyEffect,
    pub(crate) created_at: String,
}

/// The most characters one free-text policy may carry. The decision engine
/// reads the stored text as-is, so the storage bound is the prompt bound.
pub(crate) const MAX_POLICY_TEXT_CHARS: usize = 500;

/// A free-text policy the decision engine reads (docs/design/permissions.md
/// §3.1: one of the User Policies it judges an unregistered operation
/// against). It is prose, so it never gates mechanically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PolicyText {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) created_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
}

/// An approval request, immutable in the parts that identify the operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Approval {
    pub(crate) request_id: String,
    /// The agent the call belongs to (companion for the head, work session's
    /// run for the hand).
    pub(crate) session_id: String,
    pub(crate) run_id: String,
    /// The work session the call belongs to, stored when the gate issues a
    /// work approval so submission does not have to follow the run.
    pub(crate) work_id: Option<String>,
    pub(crate) action_payload_hash: Vec<u8>,
    pub(crate) tool_name: String,
    pub(crate) normalized_arguments: String,
    pub(crate) status: ApprovalStatus,
    pub(crate) created_at: String,
    pub(crate) resolved_at: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) capability: Option<String>,
}

/// What the Gate should do with a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Allow,
    HumanRequired { target: String },
    NeedsDecision { target: String },
    Deny { reason: String },
}

/// Canonical external action evaluated by `Stage::Gate`.
#[derive(Clone, Debug)]
pub(crate) enum CandidateAction {
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

/// Typed target extracted from a `CandidateAction` at the permission boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActionTarget {
    Path(String),
    Command(String),
    Url { url: String, host: Option<String> },
    Pattern(String),
    None,
}

impl ActionTarget {
    fn from_path(raw: &str) -> Self {
        let normalized = normalize_path_str(raw);
        if normalized.is_empty() {
            Self::None
        } else {
            Self::Path(normalized)
        }
    }

    fn from_url(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Self::None;
        }
        let host = url::Url::parse(trimmed)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase));
        Self::Url {
            url: trimmed.to_owned(),
            host,
        }
    }

    fn matches_pattern(&self, pattern: &str) -> bool {
        match self {
            Self::Path(path) => {
                let pat = normalize_path_str(pattern);
                !pat.is_empty() && wildcard_matches(&pat, path)
            }
            Self::Command(cmd) => {
                let pat = pattern.trim();
                !pat.is_empty() && wildcard_matches(pat, cmd.trim())
            }
            Self::Url { url, host } => {
                let pat = pattern.trim();
                !pat.is_empty()
                    && (wildcard_matches(pat, url)
                        || host
                            .as_deref()
                            .is_some_and(|h| wildcard_matches(&pat.to_ascii_lowercase(), h)))
            }
            Self::Pattern(value) => {
                let pat = pattern.trim();
                !pat.is_empty() && wildcard_matches(pat, value.trim())
            }
            Self::None => false,
        }
    }
}

fn normalize_path_str(raw: &str) -> String {
    let replaced = raw.trim().replace('\\', "/");
    replaced.strip_prefix("./").unwrap_or(&replaced).to_owned()
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return !prefix.is_empty() && value.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return !suffix.is_empty() && value.ends_with(suffix);
    }
    pattern == value
}

fn first_string_field(object: &Value, keys: &[&str]) -> Option<String> {
    let map = object.as_object()?;
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

pub(crate) fn extract_target(action: &CandidateAction) -> ActionTarget {
    match action {
        CandidateAction::DeviceCapability {
            capability, input, ..
        } => {
            if capability == "device.files"
                || matches!(
                    capability.as_str(),
                    "work.read" | "work.write" | "work.edit" | "work.delete" | "work.list"
                )
            {
                return first_string_field(input, &["path", "file", "file_path"])
                    .map_or(ActionTarget::None, |p| ActionTarget::from_path(&p));
            }
            if matches!(
                capability.as_str(),
                "work.powershell" | "work.shell" | "device.shell"
            ) {
                return first_string_field(input, &["command", "cmd"])
                    .map_or(ActionTarget::None, ActionTarget::Command);
            }
            if capability == "body.motion" {
                return first_string_field(input, &["motion"])
                    .map_or(ActionTarget::None, ActionTarget::Pattern);
            }
            if capability == "eye.watch" {
                return first_string_field(input, &["target_pattern"])
                    .map_or(ActionTarget::None, ActionTarget::Pattern);
            }
            if capability == w::COMPUTER_USE {
                return ActionTarget::None;
            }
            if let Some(url) = first_string_field(input, &["url", "uri", "endpoint"]) {
                return ActionTarget::from_url(&url);
            }
            if let Some(path) = first_string_field(input, &["path", "file", "file_path"]) {
                return ActionTarget::from_path(&path);
            }
            if let Some(cmd) = first_string_field(input, &["command", "cmd"]) {
                return ActionTarget::Command(cmd);
            }
            ActionTarget::None
        }
        CandidateAction::McpTool { input, .. } | CandidateAction::PluginTool { input, .. } => {
            if let Some(url) = first_string_field(input, &["url", "uri", "endpoint"]) {
                return ActionTarget::from_url(&url);
            }
            if let Some(path) =
                first_string_field(input, &["path", "file", "file_path", "directory", "dir"])
            {
                return ActionTarget::from_path(&path);
            }
            if let Some(cmd) = first_string_field(input, &["command", "cmd"]) {
                return ActionTarget::Command(cmd);
            }
            ActionTarget::None
        }
        CandidateAction::WebSearch { endpoint, .. } => ActionTarget::from_url(endpoint),
    }
}

/// Capability names the host never offers, whatever a client declares.
const HARD_DENIED: &[&str] = &["payment.", "securities.", "banking.", "crypto.transfer."];

const SYSTEM_MUTATION_PATHS: &[&str] = &[
    "/etc/passwd",
    "/etc/shadow",
    "/etc/sudoers",
    "/boot/",
    "/dev/sd",
    "/dev/nvme",
    "/dev/hd",
    "c:/windows/system32",
    "c:/windows/syswow64",
    "c:/windows/boot",
    "/system/library/",
];

const DESTRUCTIVE_COMMAND_TOKENS: &[&str] = &[
    "rm -rf /",
    "rm -fr /",
    "mkfs.",
    ":(){ :|:& };:",
    "format c:",
    "diskpart",
    "bcdedit /delete",
    "del /f /s /q c:\\windows",
    "del /f /s /q c:/windows",
    "remove-item -recurse -force c:\\windows",
    "remove-item -recurse -force c:/windows",
];

/// Tools that belong to the host process itself (internal conversation,
/// memory, and work management). External-effect built-ins (`device_request`,
/// `web_search`) and MCP tools are NOT host-owned and always enter `Stage::Gate`.
pub(crate) fn is_host_tool(tool_name: &str) -> bool {
    !matches!(tool_name, "device_request" | super::web_search::NAME)
        && BUILTIN_TOOLS.contains(&tool_name)
}

pub(crate) fn hard_denied(capability: &str) -> Option<&'static str> {
    HARD_DENIED
        .iter()
        .find(|prefix| capability.starts_with(**prefix))
        .copied()
}

pub(crate) fn hard_denied_action(
    action: &CandidateAction,
    target: &ActionTarget,
) -> Option<String> {
    if let CandidateAction::DeviceCapability { capability, .. } = action
        && let Some(prefix) = hard_denied(capability)
    {
        return Some(format!(
            "`{capability}` is hard-blocked by the host ({prefix}*)"
        ));
    }
    let is_file_mutation = match action {
        CandidateAction::DeviceCapability { capability, .. } => matches!(
            capability.as_str(),
            "work.write" | "work.edit" | "work.delete" | "device.files"
        ),
        CandidateAction::McpTool { tool_name, .. }
        | CandidateAction::PluginTool { tool_name, .. } => {
            let lower = tool_name.to_ascii_lowercase();
            lower.contains("write")
                || lower.contains("edit")
                || lower.contains("delete")
                || lower.contains("remove")
        }
        CandidateAction::WebSearch { .. } => false,
    };
    if is_file_mutation && let ActionTarget::Path(path) = target {
        let lower = path.to_ascii_lowercase();
        for forbidden in SYSTEM_MUTATION_PATHS {
            if lower == *forbidden
                || lower.starts_with(forbidden)
                || (forbidden.ends_with('/') && lower.starts_with(forbidden))
            {
                return Some(format!(
                    "system path `{path}` is hard-blocked from modification"
                ));
            }
        }
    }
    if let ActionTarget::Command(cmd) = target {
        let collapsed: String = cmd
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for pattern in DESTRUCTIVE_COMMAND_TOKENS {
            let hit = if *pattern == "rm -rf /" || *pattern == "rm -fr /" {
                collapsed == *pattern
                    || collapsed.starts_with(&format!("{pattern} "))
                    || collapsed.starts_with(&format!("{pattern}*"))
                    || collapsed.contains(&format!("; {pattern}"))
                    || collapsed.contains(&format!("&& {pattern}"))
            } else {
                collapsed.contains(pattern)
            };
            if hit {
                return Some(format!(
                    "destructive command pattern `{pattern}` is hard-blocked by the host"
                ));
            }
        }
        if collapsed.contains("dd ") && collapsed.contains(" of=/dev/") {
            return Some("raw block-device write via `dd of=/dev/` is hard-blocked".into());
        }
    }
    if matches!(action, CandidateAction::WebSearch { .. })
        && let ActionTarget::Url { url, host } = target
        && (url.to_ascii_lowercase().starts_with("file://")
            || host.as_deref() == Some("169.254.169.254")
            || host.as_deref() == Some("metadata.google.internal"))
    {
        return Some(format!("web_search destination `{url}` is hard-blocked"));
    }
    None
}

/// Canonical tool arguments and their SHA-256. Key order is canonical because
/// `serde_json::Value` objects are ordered maps.
pub(crate) fn canonicalise(tool_name: &str, arguments: &Value) -> (String, Vec<u8>) {
    let normalized = canonical_json(arguments);
    let hash = digest(format!("{tool_name}\n{normalized}").as_bytes());
    (normalized, hash)
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Evaluates any `CandidateAction` against Hard Deny, saved `PolicyRule`s, and
/// default approval policy (`docs/design/permissions.md` §3).
pub(crate) fn decide_action(rules: &[PolicyRule], action: &CandidateAction) -> Verdict {
    let typed_target = extract_target(action);
    if let Some(reason) = hard_denied_action(action, &typed_target) {
        return Verdict::Deny { reason };
    }
    let (tool_name, capability, server_id, raw_args, target_summary, default_approval) =
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
                    _ if capability == w::COMPUTER_USE => DefaultActionApproval::HumanRequired,
                    _ => DefaultActionApproval::NeedsDecision,
                };
                (
                    tool_name.as_str(),
                    Some(capability.as_str()),
                    None,
                    raw_arguments,
                    target(spec.as_deref(), capability, input),
                    approval,
                )
            }
            CandidateAction::McpTool {
                server_id,
                tool_name,
                input,
            } => (
                tool_name.as_str(),
                None,
                Some(server_id.as_str()),
                input,
                format!("{tool_name} {}", truncate(&input.to_string(), 200)),
                DefaultActionApproval::NeedsDecision,
            ),
            CandidateAction::PluginTool {
                plugin_id,
                tool_name,
                input,
            } => (
                tool_name.as_str(),
                Some(plugin_id.as_str()),
                None,
                input,
                format!(
                    "plugin:{plugin_id}/{tool_name} {}",
                    truncate(&input.to_string(), 180)
                ),
                DefaultActionApproval::NeedsDecision,
            ),
            CandidateAction::WebSearch {
                endpoint,
                query,
                input,
            } => {
                let host = match &typed_target {
                    ActionTarget::Url { host: Some(h), .. } => h.as_str(),
                    _ => endpoint.as_str(),
                };
                (
                    super::web_search::NAME,
                    None,
                    None,
                    input,
                    format!("web_search {host} ({})", truncate(query, 80)),
                    DefaultActionApproval::NeedsDecision,
                )
            }
        };
    let (_, raw_hash) = canonicalise(tool_name, raw_args);
    let raw_hash_hex = hex_digest(&raw_hash);
    let mut denied: Option<&PolicyRule> = None;
    let mut confirm = false;
    let mut allowed = false;
    for rule in rules.iter().filter(|rule| {
        matches_action(
            rule,
            tool_name,
            capability,
            server_id,
            raw_args,
            &raw_hash_hex,
            &typed_target,
        )
    }) {
        match rule.effect {
            PolicyEffect::Deny => {
                denied = Some(rule);
                break;
            }
            PolicyEffect::Confirm => confirm = true,
            PolicyEffect::Allow => allowed = true,
        }
    }
    if let Some(rule) = denied {
        return Verdict::Deny {
            reason: format!("saved rule `{}` denies this operation", rule.id),
        };
    }
    if confirm {
        return Verdict::HumanRequired {
            target: target_summary,
        };
    }
    if allowed {
        return Verdict::Allow;
    }
    match default_approval {
        DefaultActionApproval::Auto => Verdict::Allow,
        DefaultActionApproval::HumanRequired => Verdict::HumanRequired {
            target: target_summary,
        },
        DefaultActionApproval::NeedsDecision => Verdict::NeedsDecision {
            target: target_summary,
        },
    }
}

#[derive(Clone, Copy)]
enum DefaultActionApproval {
    Auto,
    HumanRequired,
    NeedsDecision,
}

/// The decision for a capability operation before the card is shown.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn decide_capability(
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

fn matches_action(
    rule: &PolicyRule,
    tool_name: &str,
    capability: Option<&str>,
    server_id: Option<&str>,
    raw_arguments: &Value,
    raw_hash_hex: &str,
    typed_target: &ActionTarget,
) -> bool {
    let tool_matches = rule.tool_name == "*"
        || rule.tool_name == tool_name
        || capability.is_some_and(|cap| rule.tool_name == cap)
        || server_id.is_some_and(|srv| rule.tool_name == format!("mcp:{srv}"))
        || rule.tool_name.strip_suffix('*').is_some_and(|prefix| {
            !prefix.is_empty()
                && (tool_name.starts_with(prefix)
                    || capability.is_some_and(|cap| cap.starts_with(prefix)))
        });
    if !tool_matches {
        return false;
    }
    if rule.pattern == "*" {
        return true;
    }
    if let Some(expected_hex) = rule.pattern.strip_prefix("exact_sha256:") {
        return expected_hex == raw_hash_hex;
    }
    if rule.pattern.starts_with('{') {
        return serde_json::from_str::<Value>(&rule.pattern)
            .ok()
            .is_some_and(|saved| canonical_json(&saved) == canonical_json(raw_arguments));
    }
    typed_target.matches_pattern(&rule.pattern)
}

fn target(spec: Option<&CapabilitySpec>, capability: &str, arguments: &Value) -> String {
    match spec {
        Some(spec) => super::client::approval_target(spec, arguments),
        None => format!("{capability} {}", truncate(&arguments.to_string(), 200)),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ApprovalDecision {
    Unspecified,
    Approve,
    /// Persists an exact-invocation allow rule for future matching calls.
    AlwaysApprove,
    Deny,
}

impl From<Failure> for Verdict {
    fn from(failure: Failure) -> Self {
        Self::Deny {
            reason: failure.message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(effect: PolicyEffect, tool: &str, pattern: &str) -> PolicyRule {
        PolicyRule {
            id: "r1".into(),
            tool_name: tool.into(),
            pattern: pattern.into(),
            effect,
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn spec(approval: CapabilityApproval) -> CapabilitySpec {
        CapabilitySpec {
            name: "device.files".into(),
            description: "files".into(),
            input_schema: r#"{"type":"object"}"#.into(),
            approval,
            ..Default::default()
        }
    }

    #[test]
    fn hard_denied_and_undeclared_operations_fail_safe() {
        let denied = decide_capability(
            &[rule(PolicyEffect::Allow, "*", "*")],
            Some(&spec(CapabilityApproval::Auto)),
            "payment.transfer",
            "device_request",
            &json!({}),
        );
        assert!(matches!(denied, Verdict::Deny { .. }));
        let undeclared =
            decide_capability(&[], None, "device.screen", "device_request", &json!({}));
        assert!(matches!(undeclared, Verdict::NeedsDecision { .. }));
        let auto = decide_capability(
            &[],
            Some(&spec(CapabilityApproval::Auto)),
            "body.motion",
            "body_motion",
            &json!({"motion": "nod"}),
        );
        assert_eq!(auto, Verdict::Allow);
    }

    #[test]
    fn rules_decide_by_precedence() {
        let allow = rule(PolicyEffect::Allow, "*", "*");
        let confirm = rule(PolicyEffect::Confirm, "device_request", "/tmp/x");
        let verdict = decide_capability(
            &[allow.clone(), confirm],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
        );
        assert!(matches!(verdict, Verdict::HumanRequired { .. }));
        let allow_only = decide_capability(
            std::slice::from_ref(&allow),
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/y"}}),
        );
        assert_eq!(allow_only, Verdict::Allow);
        let wildcard = decide_capability(
            &[rule(PolicyEffect::Confirm, "*", "*")],
            Some(&spec(CapabilityApproval::Auto)),
            "body.motion",
            "body_motion",
            &json!({"motion": "nod"}),
        );
        assert!(matches!(wildcard, Verdict::HumanRequired { .. }));
        let mut deny = rule(PolicyEffect::Deny, "device_request", "*");
        deny.id = "r2".into();
        let verdict = decide_capability(
            &[rule(PolicyEffect::Confirm, "device_request", "*"), deny],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
        );
        assert!(matches!(verdict, Verdict::Deny { .. }));
        // A saved allow bypasses a capability's default confirmation, but not
        // a deny or a confirm rule.
        let verdict = decide_capability(
            &[allow],
            Some(&spec(CapabilityApproval::Confirm)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/x"}}),
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn b02_rules_never_match_arbitrary_content_or_workspace_strings() {
        let allow_tmp = rule(PolicyEffect::Allow, "work.write", "/tmp/safe/*");
        let write_with_decoy_content = json!({
            "capability": "work.write",
            "input": {
                "path": "/etc/secret",
                "workspace": "/tmp/safe/ws",
                "content": "/tmp/safe/decoy"
            }
        });
        let verdict = decide_capability(
            &[allow_tmp],
            Some(&spec(CapabilityApproval::Confirm)),
            "work.write",
            "device_request",
            &write_with_decoy_content,
        );
        assert!(
            matches!(verdict, Verdict::NeedsDecision { .. }),
            "content/workspace must not trigger target rule match: {verdict:?}"
        );
    }

    #[test]
    fn b19_hard_denied_blocks_system_paths_and_destructive_commands() {
        let allow_all = rule(PolicyEffect::Allow, "*", "*");
        let sys_write = decide_capability(
            std::slice::from_ref(&allow_all),
            Some(&spec(CapabilityApproval::Auto)),
            "work.write",
            "device_request",
            &json!({"capability": "work.write", "input": {"path": "C:\\Windows\\System32\\drivers\\etc\\hosts"}}),
        );
        assert!(matches!(sys_write, Verdict::Deny { .. }));

        let nuke_cmd = decide_capability(
            std::slice::from_ref(&allow_all),
            Some(&spec(CapabilityApproval::Auto)),
            "work.powershell",
            "device_request",
            &json!({"capability": "work.powershell", "input": {"command": "rm  -rf   / --no-preserve-root"}}),
        );
        assert!(matches!(nuke_cmd, Verdict::Deny { .. }));

        let mcp_sys = decide_action(
            &[allow_all],
            &CandidateAction::McpTool {
                server_id: "fs".into(),
                tool_name: "mcp_fs_write_file".into(),
                input: json!({"path": "/etc/passwd"}),
            },
        );
        assert!(matches!(mcp_sys, Verdict::Deny { .. }));
    }

    #[test]
    fn b03_b04_mcp_and_web_search_require_gate_and_respect_rules() {
        let mcp_action = CandidateAction::McpTool {
            server_id: "notes".into(),
            tool_name: "mcp_notes_read".into(),
            input: json!({"path": "/notes/a.md"}),
        };
        assert!(matches!(
            decide_action(&[], &mcp_action),
            Verdict::NeedsDecision { .. }
        ));
        assert_eq!(
            decide_action(
                &[rule(PolicyEffect::Allow, "mcp:notes", "/notes/*")],
                &mcp_action
            ),
            Verdict::Allow
        );
        assert!(matches!(
            decide_action(
                &[rule(PolicyEffect::Confirm, "mcp_notes_read", "*")],
                &mcp_action
            ),
            Verdict::HumanRequired { .. }
        ));

        let search_action = CandidateAction::WebSearch {
            endpoint: "https://api.mwmbl.org/api/v2/search/".into(),
            query: "rust".into(),
            input: json!({"query": "rust"}),
        };
        assert!(matches!(
            decide_action(&[], &search_action),
            Verdict::NeedsDecision { .. }
        ));
        assert!(matches!(
            decide_action(
                &[rule(PolicyEffect::Deny, "web_search", "api.mwmbl.org")],
                &search_action
            ),
            Verdict::Deny { .. }
        ));
    }
}

// === Gate =================================================================

/// The most tool calls one model reply may ask for. The old plugin contract
/// enforced this per reply; the gate is the first post-decode boundary where
/// the reply's calls are visible together.
const MAX_REPLY_TOOL_CALLS: usize = 64;
/// The most bytes an id or name in a tool call may carry.
const MAX_TOOL_IDENTIFIER_BYTES: usize = 256;

fn within_tool_limits(id: &str, name: &str, reply_calls: usize) -> bool {
    id.len() <= MAX_TOOL_IDENTIFIER_BYTES
        && name.len() <= MAX_TOOL_IDENTIFIER_BYTES
        && reply_calls <= MAX_REPLY_TOOL_CALLS
}

use super::background::{self, AbortOnDropHandle};
use super::client::Client;
use super::db::Store;
use super::mind::Role;
use super::model::{self, Credentials};
use super::notify::Notifier;
use super::resources::HostRuntime;
use super::tools::run_tool;
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::{CompletionRequest, Message as CompletionMessage};
use rig_core::effect::EffectId;
use rig_core::{DynModel, operation::Completion};
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use std::collections::HashMap;
use std::time::Duration;

/// The arguments of the head's `resolve_approval` tool
/// (docs/design/permissions.md §4.2).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub(crate) struct ResolveApprovalArgs {
    /// The `request_id` of a pending approval of this companion.
    pub(crate) request_id: String,
    /// `approve` or `deny`.
    pub(crate) decision: String,
}

/// The marker the host inserts on a call whose approval was granted.
#[derive(Component)]
pub(crate) struct Approved;

/// What the call was accepted against. The device executor dispatches to it.
#[derive(Component, Clone, Debug)]
pub(crate) struct CallBinding {
    pub(crate) client_id: String,
    pub(crate) capability: String,
    pub(crate) computer_run: Option<String>,
}

/// A held tool call and the marker that keeps it from running.
#[derive(Clone, Copy)]
pub(crate) struct Hold {
    pub(crate) call: Entity,
    pub(crate) hold: Entity,
}

#[derive(Clone, Debug)]
pub(crate) struct SubmitOutcome {
    pub(crate) approval: Approval,
    pub(crate) rule_saved: bool,
    pub(crate) rule_error: Option<String>,
}

/// The approvals that are currently attached to a live tool call.
#[derive(Resource, Default)]
pub(crate) struct Approvals {
    pub(crate) by_request: HashMap<String, Hold>,
}

/// The saved policy rules and the gate system.
pub(crate) struct GatePlugin;

impl Plugin for GatePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Decisions>()
            .add_systems(AgentPass, gate.in_set(Stage::Gate))
            .add_systems(
                Update,
                (expire_orphaned_approvals, collect_decisions)
                    .chain()
                    .after(AgentLoop),
            )
            .add_open_tool::<ResolveApprovalArgs, _>(
                "resolve_approval",
                "Approve or deny one pending approval request of this companion. Call it when \
                 the user has agreed or refused in conversation to the operation the approval \
                 card names. Arguments: request_id, decision (`approve` or `deny`).",
                ToolOptions::default(),
                run_tool(resolve_approval),
            );
    }
}

/// Applies the decision pipeline to every tool call that is about to start.
/// Internal host tools pass; device capabilities, MCP tools, and web_search
/// enter `Stage::Gate`, are checked against saved rules and, when needed, held
/// for Decision API evaluation or user approval.
fn gate(world: &mut World) {
    let calls: Vec<(Entity, String, String, Value, Entity, Option<EffectId>)> = {
        let mut query = world.query_filtered::<
            (Entity, &ToolRequest, &ForAgent, Option<&Effect>),
            (ReadyToStart, ToolCalls),
        >();
        query
            .iter(world)
            .filter(|(call, _, _, _)| world.get::<Approved>(*call).is_none())
            .filter(|(call, _, _, _)| {
                world
                    .get::<Decision>(*call)
                    .is_none_or(|decision| decision.get().is_none())
            })
            .map(|(call, request, agent, effect)| {
                (
                    call,
                    request.call.id.wire().into_owned(),
                    request.call.function.name.as_str().to_owned(),
                    Value::Object(request.call.function.arguments.clone()),
                    agent.0,
                    effect.and_then(|effect| effect.parent),
                )
            })
            .collect()
    };
    if calls.is_empty() {
        return;
    }
    let mut reply_calls: HashMap<EffectId, usize> = HashMap::new();
    {
        let mut spawned = world.query::<(&Effect, &ToolRequest)>();
        for (effect, _) in spawned.iter(world) {
            if let Some(parent) = effect.parent {
                *reply_calls.entry(parent).or_default() += 1;
            }
        }
    }
    let store = world.resource::<Store>().clone();
    let mcp_tools = crate::host::mcp::tool_servers(world);
    let plugin_tools = crate::host::providers::extensions::tool_owners(world);
    let mut cached_rules: Option<Vec<PolicyRule>> = None;
    for (call, call_id, tool_name, arguments, agent, parent) in calls {
        let asked = parent
            .and_then(|parent| reply_calls.get(&parent).copied())
            .unwrap_or(0);
        if !within_tool_limits(&call_id, &tool_name, asked) {
            refuse(
                world,
                call,
                "a reply exceeded the tool-call count or identifier length limit",
            );
            continue;
        }
        if is_host_tool(&tool_name) {
            continue;
        }
        let rules = if let Some(rules) = &cached_rules {
            rules.clone()
        } else {
            match store.policy_rules() {
                Ok(rules) => {
                    cached_rules = Some(rules.clone());
                    rules
                }
                Err(error) => {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "policy rules could not be read; failing closed",
                    );
                    continue;
                }
            }
        };
        let (companion_id, work_id, run_id) = binding_of(world, agent);
        if let Some(providers) = world.get_resource::<super::providers::Providers>().cloned()
            && !providers
                .with_extension(ene_plugin::Extension::Hook)
                .is_empty()
            && let Some(runtime) = world
                .get_resource::<super::resources::HostRuntime>()
                .map(|r| r.0.clone())
        {
            let hook_store = store.clone();
            let hook_event = ene_plugin::HookEvent::PreTool {
                companion_id: companion_id.clone(),
                tool_name: tool_name.clone(),
                arguments: arguments.clone(),
            };
            let outcome = tokio::task::block_in_place(|| {
                runtime.block_on(super::providers::extensions::run_hooks(
                    &providers,
                    &hook_store,
                    hook_event,
                ))
            });
            if let Some(reason) = outcome.blocked {
                refuse(world, call, &reason);
                continue;
            }
        }
        let (action, bound_client_id, bound_capability) = if let Some(server_id) =
            mcp_tools.get(&tool_name)
        {
            (
                CandidateAction::McpTool {
                    server_id: server_id.clone(),
                    tool_name: tool_name.clone(),
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else if let Some(plugin_id) = plugin_tools.get(&tool_name) {
            (
                CandidateAction::PluginTool {
                    plugin_id: plugin_id.clone(),
                    tool_name: tool_name.clone(),
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else if tool_name == super::web_search::NAME {
            let providers = world
                .get_resource::<super::providers::Providers>()
                .cloned()
                .unwrap_or_default();
            let resolved = match super::web_search::resolve(
                &store,
                world.get_resource::<super::model::Credentials>(),
                &providers,
            ) {
                Ok(resolved) => resolved,
                Err(reason) => {
                    refuse(world, call, &reason);
                    continue;
                }
            };
            let query = arguments
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            (
                CandidateAction::WebSearch {
                    endpoint: resolved.endpoint,
                    query,
                    input: arguments.clone(),
                },
                None,
                None,
            )
        } else {
            let client_id = match store.target(&companion_id, work_id.as_deref()) {
                Ok(Some(client_id)) => client_id,
                Ok(None) => {
                    refuse(
                        world,
                        call,
                        "no_active_client: no client is bound to this agent",
                    );
                    continue;
                }
                Err(error) => {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "client binding could not be read; failing closed",
                    );
                    continue;
                }
            };
            let client = match store.client(&client_id) {
                Ok(Some(client)) => client,
                Ok(None) => {
                    refuse(world, call, "the bound client is not registered");
                    continue;
                }
                Err(error) => {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                    refuse(
                        world,
                        call,
                        "client record could not be read; failing closed",
                    );
                    continue;
                }
            };
            let resolved = match resolve_capability(world, agent, &client, &tool_name, &arguments) {
                Ok(resolved) => resolved,
                Err(error) => {
                    refuse(world, call, error.message());
                    continue;
                }
            };
            let Some((capability, spec)) = resolved else {
                refuse(world, call, &format!("unknown device tool `{tool_name}`"));
                continue;
            };
            if !client.is_granted(&capability) {
                refuse(
                    world,
                    call,
                    &format!("capability `{capability}` is not granted"),
                );
                continue;
            }
            let empty = serde_json::json!({});
            let input = if tool_name == "device_request" {
                arguments.get("input").unwrap_or(&empty).clone()
            } else {
                arguments.clone()
            };
            if (capability.starts_with(super::client::WORKSPACE_CAPABILITY_PREFIX)
                || capability == w::COMPUTER_USE)
                && let Some(spec) = &spec
                && let Err(error) = super::client::validate_input(spec, &input)
            {
                refuse(world, call, error.message());
                continue;
            }
            (
                CandidateAction::DeviceCapability {
                    capability: capability.clone(),
                    spec: spec.map(Box::new),
                    tool_name: tool_name.clone(),
                    input,
                    raw_arguments: arguments.clone(),
                },
                Some(client_id),
                Some(capability),
            )
        };
        let verdict = decide_action(&rules, &action);
        if let (Some(client_id), Some(capability)) =
            (bound_client_id.clone(), bound_capability.clone())
        {
            world.entity_mut(call).insert(CallBinding {
                client_id,
                capability: capability.clone(),
                computer_run: (capability == w::COMPUTER_USE).then(|| run_id.clone()),
            });
        }
        match verdict {
            Verdict::Allow => {}
            Verdict::Deny { reason } => refuse(world, call, &reason),
            Verdict::HumanRequired { target } => {
                let request_id = uuid::Uuid::new_v4().to_string();
                let (normalized, hash) = canonicalise(&tool_name, &arguments);
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    work_id: work_id.clone(),
                    action_payload_hash: hash,
                    tool_name: tool_name.clone(),
                    normalized_arguments: normalized,
                    status: ApprovalStatus::Pending,
                    created_at: super::db::now(),
                    resolved_at: None,
                    client_id: bound_client_id,
                    capability: bound_capability,
                };
                if let Err(error) = issue_hold(world, &store, approval, call) {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                    refuse(world, call, "approval persistence failed; failing closed");
                    return;
                }
                let pending = Pending {
                    request: request_id,
                    companion: companion_id.clone(),
                    target,
                };
                push_card(world, &pending);
            }
            Verdict::NeedsDecision { target } => {
                let request_id = uuid::Uuid::new_v4().to_string();
                let (normalized, hash) = canonicalise(&tool_name, &arguments);
                let arguments_view = truncate(&normalized, DECISION_ARGUMENT_CHARS);
                let approval = Approval {
                    request_id: request_id.clone(),
                    session_id: companion_id.clone(),
                    run_id: run_id.clone(),
                    work_id: work_id.clone(),
                    action_payload_hash: hash,
                    tool_name: tool_name.clone(),
                    normalized_arguments: normalized,
                    status: ApprovalStatus::Pending,
                    created_at: super::db::now(),
                    resolved_at: None,
                    client_id: bound_client_id,
                    capability: bound_capability,
                };
                if let Err(error) = issue_hold(world, &store, approval, call) {
                    if error.is_storage() {
                        super::fail(world, error.to_string());
                        return;
                    }
                    refuse(world, call, "approval persistence failed; failing closed");
                    return;
                }
                let pending = Pending {
                    request: request_id,
                    companion: companion_id.clone(),
                    target: target.clone(),
                };
                let policies = match store.policy_texts() {
                    Ok(policies) => policies,
                    Err(error) => {
                        if error.is_storage() {
                            super::fail(world, error.to_string());
                            return;
                        }
                        push_card(world, &pending);
                        continue;
                    }
                };
                let inputs = DecisionInputs {
                    tool_name: tool_name.clone(),
                    target,
                    arguments: arguments_view,
                    request: request_context(world, &store, agent, &run_id),
                    policies,
                    rules: rules.clone(),
                };
                start_decision(world, &store, pending, inputs);
            }
        }
    }
}

pub(crate) fn issue_hold(
    world: &mut World,
    store: &Store,
    approval: Approval,
    call: Entity,
) -> Result<Hold> {
    let request_id = approval.request_id.clone();
    store.insert_approval(&approval)?;
    let hold = world.spawn(HoldOf(call)).id();
    let entry = Hold { call, hold };
    world
        .resource_mut::<Approvals>()
        .by_request
        .insert(request_id, entry);
    Ok(entry)
}

pub(crate) fn notify_approval_card(
    world: &mut World,
    companion: &str,
    approval: &Approval,
    target: &str,
) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
            request: Some(crate::host::wire::approval(approval)).into(),
            ..Default::default()
        })),
    );
    notifier.notice(
        Some(companion),
        "approval_required",
        &format!("{target}: approval is required before this operation runs"),
    );
}

/// The companion, work session and run an agent serves. A work agent carries
/// both ids; a companion agent leaves the work ones empty.
fn binding_of(world: &World, agent: Entity) -> (String, Option<String>, String) {
    if let Some(companion) = world.get::<super::agent::CompanionAgent>(agent) {
        return (companion.companion_id.clone(), None, String::new());
    }
    match world.get::<super::agent::WorkAgent>(agent) {
        Some(work) => (
            work.companion_id.clone(),
            Some(work.work_id.clone()),
            work.run_id.clone(),
        ),
        None => (String::new(), None, String::new()),
    }
}

/// The capability a device tool call names, with its declaration.
/// The client is already resolved, so the hand resolves against the client it
/// is bound to, not against the companion's active client. A capability tool
/// is parent-only and must be published for the calling agent's bound client;
/// `client::dynamic::tool_spec_for` resolves that binding per agent, so one
/// published definition can serve several companions. A call that no longer
/// matches is refused with a reason instead of being treated as an unknown
/// tool.
fn resolve_capability(
    world: &mut World,
    agent: Entity,
    client: &Client,
    tool_name: &str,
    arguments: &Value,
) -> Result<Option<(String, Option<CapabilitySpec>)>> {
    if tool_name == "device_request" {
        let Some(capability) = arguments.get("capability").and_then(Value::as_str) else {
            return Err(HostError::Invalid(
                ("device_request requires a capability name as a string").into(),
            ));
        };
        if !client.capabilities.iter().any(|name| name == capability) {
            return Err(HostError::Invalid(format!(
                "capability `{capability}` is not declared by the bound client"
            )));
        }
        if capability.starts_with(super::client::WORKSPACE_CAPABILITY_PREFIX)
            && world.get::<super::agent::WorkAgent>(agent).is_none()
        {
            return Err(HostError::Permission(
                ("workspace capabilities require a work session").into(),
            ));
        }
        if capability == w::COMPUTER_USE
            && (world.get::<super::agent::WorkAgent>(agent).is_none()
                || !super::client::computer::model_supported(
                    world.get::<rig_ecs::model::Connection>(agent),
                ))
        {
            return Err(HostError::Permission(
                ("Computer Use requires an internal work agent with an image and tool capable task model").into(),
            ));
        }
        return Ok(Some((
            capability.to_owned(),
            client.spec(capability).cloned(),
        )));
    }
    // Declared capability tools are offered to the parent only; the hand uses
    // the generic dispatcher so its work-session binding stays explicit.
    if world.get::<super::agent::WorkAgent>(agent).is_some() {
        return Err(HostError::Permission(
            ("capability tools are offered to the parent only").into(),
        ));
    }
    let Some(spec) = super::client::dynamic::tool_spec_for(world, client, tool_name)? else {
        return Ok(None);
    };
    Ok(Some((spec.name.clone(), Some(spec))))
}

// === Decision Engine =======================================================

/// How long the decision model may take. The docs call the Decision API
/// 軽量・高速 (docs/features/settings/permissions.md): the held call waits on
/// it, so the budget is short — the same one the host gives a connection
/// probe — and a timeout fails safe to the card instead of stalling the
/// operation (docs/design/permissions.md §3.1).
const DECISION_TIMEOUT: Duration = Duration::from_secs(15);

/// The most decisions one host runs at once. One reply can hold dozens of
/// calls, and a flood of model calls must not queue them all; beyond the cap
/// the card is the honest answer.
const MAX_IN_FLIGHT_DECISIONS: usize = 8;

/// The per-string bounds the decision prompt enforces
/// (docs/design/permissions.md §3.1: 入力遮断 — bounded inputs only).
const DECISION_ARGUMENT_CHARS: usize = 2_000;
const DECISION_REQUEST_CHARS: usize = 1_000;
const DECISION_RULE_CHARS: usize = 200;
const DECISION_REASON_CHARS: usize = 300;
/// The most policies and rules each prompt lists.
const DECISION_LIST: usize = 32;

/// The bounded inputs one decision reads. The candidate action, the user's
/// request, the user's policies and the saved rules — never the agent's
/// chain-of-thought or raw external data (docs §3.1).
struct DecisionInputs {
    tool_name: String,
    /// The capability target string, the same one the card would show.
    target: String,
    /// Canonical arguments, truncated.
    arguments: String,
    /// The user's request: the companion's newest user message, or the run's
    /// instruction for a hand.
    request: String,
    policies: Vec<PolicyText>,
    rules: Vec<PolicyRule>,
}

/// One decision the gate asked for: the approval it decides and the card
/// context it fails safe to.
struct Pending {
    request: String,
    companion: String,
    target: String,
}

/// One decision in flight: what it decides, the task, and the model that
/// runs it (the engine the resolution log names).
struct InFlight {
    pending: Pending,
    task: AbortOnDropHandle<DecisionOutcome>,
    engine: String,
}

type DecisionOutcome = std::result::Result<ModelDecision, String>;

/// One answer from the decision engine, with the short reason it gave.
struct ModelDecision {
    allow: bool,
    reason: String,
}

/// Background decision state keyed by request id: one decision per pending
/// approval, at most [`MAX_IN_FLIGHT_DECISIONS`] on the runtime at once. The
/// same take-finished shape as `CompactionState`.
#[derive(Resource, Default)]
pub(crate) struct Decisions {
    running: HashMap<String, InFlight>,
}

impl Decisions {
    fn is_running(&self, request: &str) -> bool {
        self.running.contains_key(request)
    }

    fn count(&self) -> usize {
        self.running.len()
    }

    fn start(
        &mut self,
        pending: Pending,
        task: AbortOnDropHandle<DecisionOutcome>,
        engine: String,
    ) {
        debug_assert!(
            !self.is_running(&pending.request),
            "one decision per request"
        );
        self.running.insert(
            pending.request.clone(),
            InFlight {
                pending,
                task,
                engine,
            },
        );
    }

    /// Takes the one finished task, with the approval it decides.
    fn take_finished(&mut self) -> Option<InFlight> {
        let request = self
            .running
            .iter()
            .find_map(|(request, flight)| flight.task.is_finished().then(|| request.clone()))?;
        self.running.remove(&request)
    }
}

/// The decision engine's instruction, in the same voice as the summarizer's
/// (host/compaction.rs): judge only, answer in the one JSON shape, never
/// continue the chat.
const DECISION_SYSTEM_PROMPT: &str = "あなたはデスクトップ常駐AIコンパニオンの権限判定エンジンです。機械的な許可ルールを通らず保留中の端末操作を、依頼文脈・ユーザーの自由文ポリシー・保存済みルールに照らして判定します。操作が安全で依頼とポリシーに適合するなら allow、危険・ポリシー抵触・依頼文脈と合わない・判断に迷うなら require_approval としてください。判定だけを次のJSON形式で返し、それ以外の文章は一切書かないでください:\n{\"decision\": \"allow\" | \"require_approval\", \"reason\": \"一行の短い理由\"}";

/// The one question the decision model gets. Every string is bounded, every
/// list capped, so the prompt is the whole input surface.
fn decision_prompt(inputs: &DecisionInputs) -> String {
    let mut prompt = format!(
        "[依頼]\n{}\n\n[操作]\nツール: {}\n対象: {}\n引数: {}",
        or_none(&inputs.request),
        truncate(&inputs.tool_name, MAX_TOOL_IDENTIFIER_BYTES),
        truncate(&inputs.target, DECISION_ARGUMENT_CHARS),
        truncate(&inputs.arguments, DECISION_ARGUMENT_CHARS),
    );
    prompt.push_str("\n\n[ユーザーの自由文ポリシー]");
    let policies: Vec<_> = inputs.policies.iter().take(DECISION_LIST).collect();
    if policies.is_empty() {
        prompt.push_str("\n(なし)");
    } else {
        for policy in policies {
            prompt.push_str(&format!(
                "\n- {}",
                truncate(&policy.text, MAX_POLICY_TEXT_CHARS)
            ));
        }
    }
    prompt.push_str("\n\n[保存済みルール（参考）]");
    let rules: Vec<_> = inputs.rules.iter().take(DECISION_LIST).collect();
    if rules.is_empty() {
        prompt.push_str("\n(なし)");
    } else {
        for rule in rules {
            prompt.push_str(&format!(
                "\n- {} / {} → {}",
                truncate(&rule.tool_name, MAX_TOOL_IDENTIFIER_BYTES),
                truncate(&rule.pattern, DECISION_RULE_CHARS),
                rule.effect.as_str(),
            ));
        }
    }
    prompt
}

fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(なし)"
    } else {
        text
    }
}

/// The decision engine's reply: strict JSON, both fields required, nothing
/// else accepted.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionReply {
    decision: String,
    reason: String,
}

/// Asks the decision model one question and parses its strict JSON reply.
/// Every input was bounded before the spawn, so this never reads the world.
async fn run_decision(model: DynModel<Completion>, inputs: DecisionInputs) -> DecisionOutcome {
    let request = CompletionRequest::new(decision_prompt(&inputs)).preamble(DECISION_SYSTEM_PROMPT);
    let response = tokio::time::timeout(DECISION_TIMEOUT, model.call(request))
        .await
        .map_err(|_| "the decision model timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    let reply: DecisionReply = serde_json::from_str(response.text().trim())
        .map_err(|_| "the decision reply was not the strict JSON asked for".to_owned())?;
    let allow = match reply.decision.as_str() {
        "allow" => true,
        "require_approval" => false,
        _ => return Err("the decision reply named an unknown verdict".to_owned()),
    };
    Ok(ModelDecision {
        allow,
        reason: truncate(reply.reason.trim(), DECISION_REASON_CHARS),
    })
}

/// The user's request the decision judges the operation against: the run's
/// instruction for a hand, else the companion's newest user message.
fn request_context(world: &World, store: &Store, agent: Entity, run_id: &str) -> String {
    if !run_id.is_empty()
        && let Ok(Some(run)) = store.run(run_id)
    {
        return truncate(&run.instruction, DECISION_REQUEST_CHARS);
    }
    world
        .get::<Conversation>(agent)
        .map(latest_user_text)
        .unwrap_or_default()
}

/// The newest non-empty user message of a conversation.
fn latest_user_text(conversation: &Conversation) -> String {
    conversation
        .entries()
        .rev()
        .filter(|entry| matches!(entry.message, CompletionMessage::User { .. }))
        .find_map(|entry| super::agent::entry_text(&entry))
        .map(|text| truncate(&text, DECISION_REQUEST_CHARS))
        .unwrap_or_default()
}

/// Starts the background decision for a fresh pending approval, or shows the
/// card straight away when the decision cannot run — no model, no runtime, no
/// free slot: every failure is the user's call, never a silent run
/// (docs/features/settings/permissions.md: 失敗時は安全側).
fn start_decision(world: &mut World, store: &Store, pending: Pending, inputs: DecisionInputs) {
    let Some(runtime) = world.get_resource::<HostRuntime>().map(|r| r.0.clone()) else {
        push_card(world, &pending);
        return;
    };
    let (Some(credentials), Some(models)) = (
        world.get_resource::<Credentials>().cloned(),
        world.get_resource::<Models>(),
    ) else {
        push_card(world, &pending);
        return;
    };
    let busy = {
        let decisions = world.resource::<Decisions>();
        decisions.is_running(&pending.request) || decisions.count() >= MAX_IN_FLIGHT_DECISIONS
    };
    if busy {
        push_card(world, &pending);
        return;
    }
    let Ok(Some(companion)) = store.companion(&pending.companion) else {
        push_card(world, &pending);
        return;
    };
    let providers = world
        .get_resource::<super::providers::Providers>()
        .cloned()
        .unwrap_or_default();
    // The Decision role falls back to the task model when unassigned; a
    // connect failure fails safe to the card.
    let model = match model::connect(
        store,
        &credentials,
        models.0.catalog(),
        &companion,
        Role::Decision,
        &providers,
    ) {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(
                companion = %pending.companion,
                %error,
                "no decision model answers; the card is shown"
            );
            push_card(world, &pending);
            return;
        }
    };
    let engine = model.spec().map(|spec| spec.id.clone()).unwrap_or_default();
    let wake = world.get_resource::<Wake>().cloned();
    let task = background::spawn(&runtime, wake.as_ref(), run_decision(model, inputs));
    world
        .resource_mut::<Decisions>()
        .start(pending, task, engine);
}

/// Lands one finished decision. An allow resolves the approval exactly as
/// the user's approve does (`submit`), with a log naming the engine; a
/// require-approval answer, a timeout, an invalid reply or a failed call
/// shows the card the gate would have shown. A decision that arrives after
/// the approval was answered or expired drops (docs/design/permissions.md
/// §4.2: an approval is resolved once).
fn collect_decisions(world: &mut World) {
    let Some(flight) = world.resource_mut::<Decisions>().take_finished() else {
        return;
    };
    let request = flight.pending.request.clone();
    let outcome = background::take(flight.task)
        .unwrap_or_else(|| Err("the decision task ended without a result".to_owned()));
    let store = world.resource::<Store>().clone();
    let approval = match store.approval(&request) {
        Ok(Some(approval)) => approval,
        Ok(None) => {
            tracing::debug!(request = %request, "a late decision dropped: no such approval");
            return;
        }
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
                return;
            }
            tracing::warn!(%error, request = %request, "decision collection failed to read approval");
            push_card(world, &flight.pending);
            return;
        }
    };
    if approval.status != ApprovalStatus::Pending {
        tracing::debug!(
            request = %request,
            "a late decision dropped: the approval was already answered"
        );
        return;
    }
    match outcome {
        Ok(decision) if decision.allow => {
            match submit(world, &request, ApprovalDecision::Approve) {
                Ok(_) => tracing::info!(
                    companion = %flight.pending.companion,
                    engine = %flight.engine,
                    reason = %decision.reason,
                    request = %request,
                    "the decision engine allowed the operation"
                ),
                Err(error) => tracing::debug!(
                    %error,
                    request = %request,
                    "a late decision dropped: the approval was already answered"
                ),
            }
        }
        Ok(decision) => {
            tracing::info!(
                companion = %flight.pending.companion,
                engine = %flight.engine,
                reason = %decision.reason,
                request = %request,
                "the decision engine requires the user's approval"
            );
            push_card(world, &flight.pending);
        }
        Err(why) => {
            tracing::warn!(%why, request = %request, "the decision failed; the card is shown");
            push_card(world, &flight.pending);
        }
    }
}

/// The approval card and its notice, the ones the Confirm branch pushed
/// before the Decision API. Only a still-pending approval is carded.
fn push_card(world: &mut World, pending: &Pending) {
    let store = world.resource::<Store>().clone();
    let approval = match store.approval(&pending.request) {
        Ok(Some(approval)) => approval,
        Ok(None) => return,
        Err(error) => {
            if error.is_storage() {
                super::fail(world, error.to_string());
            } else {
                tracing::warn!(%error, request = %pending.request, "failed to read approval for card");
            }
            return;
        }
    };
    if approval.status != ApprovalStatus::Pending {
        return;
    }
    notify_approval_card(world, &pending.companion, &approval, &pending.target);
}

pub(super) fn decide(world: &mut World, call: Entity, outcome: Outcome) {
    if let Some(mut decision) = world.get_mut::<Decision>(call) {
        decision.decide(Decider::plugin("ene-host"), outcome);
    }
}

/// Answers a tool call with a successful result.
pub(crate) fn answer(world: &mut World, call: Entity, text: &str) {
    decide(
        world,
        call,
        Outcome::from(rig_core::tool::ToolResult::success(text.into())),
    );
}

/// Answers a tool call with a refusal.
pub(crate) fn refuse(world: &mut World, call: Entity, reason: &str) {
    decide(world, call, Outcome::refused(reason));
}

/// Takes a held call out of the map and despawns its hold marker.
fn release(world: &mut World, request: &str) -> Option<Hold> {
    let hold = world
        .resource_mut::<Approvals>()
        .by_request
        .remove(request)?;
    if world.get_entity(hold.hold).is_ok() {
        world.entity_mut(hold.hold).despawn();
    }
    Some(hold)
}

/// The client an approval is bound to. The head follows the companion's active
/// client; the hand follows the work session stored with the approval, or its
/// run. A move never voids the approval.
fn bound_client(store: &Store, approval: &Approval) -> Result<Option<String>> {
    if let Some(work_id) = approval.work_id.as_deref().filter(|id| !id.is_empty()) {
        let Some(session) = store.work_session(work_id)? else {
            return Ok(None);
        };
        if session.companion_id != approval.session_id {
            return Ok(None);
        }
        return Ok(session.client_id);
    }
    if approval.run_id.is_empty() {
        return Ok(store
            .activation(&approval.session_id)?
            .and_then(|activation| activation.client_id));
    }
    let Some(run) = store.run(&approval.run_id)? else {
        return Ok(None);
    };
    let Some(session) = store.work_session(&run.work_id)? else {
        return Ok(None);
    };
    if session.companion_id != approval.session_id {
        return Ok(None);
    }
    Ok(session.client_id)
}

/// Approves or denies a pending request. Validates hash, client binding,
/// grants, and DB state before mutating the ECS hold or granting execution.
pub(crate) fn submit(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> crate::host::error::Result<Approval> {
    submit_with_outcome(world, request_id, decision).map(|outcome| outcome.approval)
}

pub(crate) fn submit_with_outcome(
    world: &mut World,
    request_id: &str,
    decision: ApprovalDecision,
) -> crate::host::error::Result<SubmitOutcome> {
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval(request_id)?
        .ok_or_else(|| HostError::NotFound(("approval not found").into()))?;
    if approval.status != ApprovalStatus::Pending {
        return Err(HostError::Conflict(
            ("approval is no longer pending").into(),
        ));
    }
    let Some(hold) = world
        .resource::<Approvals>()
        .by_request
        .get(request_id)
        .copied()
    else {
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
        return Err(HostError::Conflict(
            ("approval is not attached to a live tool call").into(),
        ));
    };
    let request_snapshot = world.get::<ToolRequest>(hold.call).map(|request| {
        (
            request.call.function.name.as_str().to_owned(),
            Value::Object(request.call.function.arguments.clone()),
        )
    });
    let changed = request_snapshot
        .as_ref()
        .map(|(name, args)| canonicalise(name, args).1)
        .is_none_or(|hash| hash != approval.action_payload_hash);
    if changed {
        store.resolve_approval(request_id, ApprovalStatus::Expired)?;
        release(world, request_id);
        refuse(
            world,
            hold.call,
            "the operation arguments changed after the approval was requested",
        );
        return Err(HostError::Conflict(
            ("approval no longer matches the call arguments").into(),
        ));
    }
    match decision {
        ApprovalDecision::Approve | ApprovalDecision::AlwaysApprove => {
            let call_binding = world.get::<CallBinding>(hold.call).cloned();
            let mut rebound_client: Option<String> = None;
            if let Some(binding) = &call_binding {
                let target_id =
                    bound_client(&store, &approval)?.unwrap_or_else(|| binding.client_id.clone());
                let client = store.client(&target_id)?.ok_or_else(|| {
                    HostError::Conflict(("the bound client is not registered").into())
                })?;
                if !client.is_granted(&binding.capability) {
                    return Err(HostError::Conflict(format!(
                        "capability `{}` is no longer granted on `{target_id}`",
                        binding.capability
                    )));
                }
                if let Some((tool_name, raw_args)) = &request_snapshot {
                    let empty = serde_json::json!({});
                    let input = if tool_name == "device_request" {
                        raw_args.get("input").unwrap_or(&empty).clone()
                    } else {
                        raw_args.clone()
                    };
                    let action = CandidateAction::DeviceCapability {
                        capability: binding.capability.clone(),
                        spec: client.spec(&binding.capability).cloned().map(Box::new),
                        tool_name: tool_name.clone(),
                        input,
                        raw_arguments: raw_args.clone(),
                    };
                    let typed_target = extract_target(&action);
                    if let Some(reason) = hard_denied_action(&action, &typed_target) {
                        store.resolve_approval(request_id, ApprovalStatus::Denied)?;
                        release(world, request_id);
                        refuse(world, hold.call, &reason);
                        return Err(HostError::Permission(reason));
                    }
                    if approval.client_id.as_deref() != Some(target_id.as_str()) {
                        let rules = store.policy_rules()?;
                        if let Verdict::Deny { reason } = decide_action(&rules, &action) {
                            store.resolve_approval(request_id, ApprovalStatus::Denied)?;
                            release(world, request_id);
                            refuse(world, hold.call, &reason);
                            return Err(HostError::Permission(reason));
                        }
                    }
                }
                rebound_client = Some(target_id);
            }
            let approval = store.resolve_approval_with_client(
                request_id,
                ApprovalStatus::Approved,
                rebound_client.as_deref(),
            )?;
            release(world, request_id);
            if let Some(target_id) = rebound_client
                && let Some(mut binding) = world.get_mut::<CallBinding>(hold.call)
            {
                binding.client_id = target_id;
            }
            world.entity_mut(hold.call).insert(Approved);
            let mut rule_saved = false;
            let mut rule_error = None;
            if matches!(decision, ApprovalDecision::AlwaysApprove) {
                let pattern = if approval.normalized_arguments.trim().is_empty() {
                    "*".to_owned()
                } else if approval.normalized_arguments.chars().count() <= 4_000 {
                    approval.normalized_arguments.clone()
                } else {
                    format!("exact_sha256:{}", hex_digest(&approval.action_payload_hash))
                };
                let rule = PolicyRule {
                    id: uuid::Uuid::new_v4().to_string(),
                    tool_name: approval.tool_name.clone(),
                    pattern,
                    effect: PolicyEffect::Allow,
                    created_at: String::new(),
                };
                match store.apply_policy_rules(&[rule], &[]) {
                    Ok(rules) => {
                        rule_saved = true;
                        let wire_rules = rules.iter().map(crate::host::wire::policy_rule).collect();
                        let mut notifier = world.resource_mut::<Notifier>();
                        notifier.push(
                            None,
                            None,
                            w::notification::Event::PolicyRulesUpdated(Box::new(
                                w::PolicyRulesUpdated {
                                    rules: wire_rules,
                                    ..Default::default()
                                },
                            )),
                        );
                    }
                    Err(error) => {
                        if error.is_storage() {
                            super::fail(world, error.to_string());
                        }
                        let msg = error.to_string();
                        tracing::warn!(%error, "failed to persist AlwaysApprove policy rule");
                        let mut notifier = world.resource_mut::<Notifier>();
                        notifier.notice(
                            Some(&approval.session_id),
                            "policy_rule_save_failed",
                            &format!(
                                "承認は実行されましたが、常時許可ルールの保存に失敗しました: {msg}"
                            ),
                        );
                        rule_error = Some(msg);
                    }
                }
            }
            notify_resolved(world, &approval);
            Ok(SubmitOutcome {
                approval,
                rule_saved,
                rule_error,
            })
        }
        ApprovalDecision::Deny | ApprovalDecision::Unspecified => {
            let approval = store.resolve_approval(request_id, ApprovalStatus::Denied)?;
            release(world, request_id);
            refuse(world, hold.call, "the user denied this operation");
            notify_resolved(world, &approval);
            Ok(SubmitOutcome {
                approval,
                rule_saved: false,
                rule_error: None,
            })
        }
    }
}

fn notify_resolved(world: &mut World, approval: &Approval) {
    let mut notifier = world.resource_mut::<Notifier>();
    notifier.notice(
        Some(&approval.session_id),
        "approval_resolved",
        &format!("{}: {}", approval.request_id, approval.status.as_str()),
    );
}

/// The head's `resolve_approval` tool: it answers one pending approval of its
/// own companion from the conversation, instead of the user clicking the card
/// (docs/design/permissions.md §4.2). Another companion's request and an
/// already-resolved one are refused.
pub(crate) fn resolve_approval(
    world: &mut World,
    agent: Entity,
    _call_id: &str,
    args: ResolveApprovalArgs,
) -> Result<String> {
    let companion_id = world
        .get::<super::agent::CompanionAgent>(agent)
        .map(|companion| companion.companion_id.clone())
        .ok_or_else(|| HostError::Internal(("this tool belongs to a companion").into()))?;
    let decision = match args.decision.trim() {
        "approve" => ApprovalDecision::Approve,
        "deny" => ApprovalDecision::Deny,
        other => {
            return Err(HostError::Invalid(format!(
                "decision must be `approve` or `deny`, not `{other}`"
            )));
        }
    };
    let store = world.resource::<Store>().clone();
    let approval = store
        .approval(&args.request_id)?
        .ok_or_else(|| HostError::NotFound(("approval not found").into()))?;
    if approval.session_id != companion_id {
        return Err(HostError::Permission(
            ("this approval belongs to another companion").into(),
        ));
    }
    let approval = submit(world, &args.request_id, decision)?;
    Ok(serde_json::json!({
        "request_id": approval.request_id,
        "status": approval.status.as_str(),
    })
    .to_string())
}

/// A stopped turn takes its held calls with it; the approval must not become
/// reusable.
fn expire_orphaned_approvals(world: &mut World) {
    let orphans: Vec<String> = {
        let approvals = world.resource::<Approvals>();
        approvals
            .by_request
            .iter()
            .filter(|(_, hold)| world.get_entity(hold.call).is_err())
            .map(|(request, _)| request.clone())
            .collect()
    };
    for request in orphans {
        release(world, &request);
        if let Ok(store) = world.resource::<Store>().approval(&request)
            && store.is_some_and(|approval| approval.status == ApprovalStatus::Pending)
        {
            let store = world.resource::<Store>().clone();
            let _ = store.resolve_approval(&request, ApprovalStatus::Expired);
        }
    }
}

/// RFC 8785 (JCS) canonical JSON: the approval hash must not depend on
/// object key order. serde_json_canonicalizer sorts keys by UTF-16 code
/// units and uses the standard number form, so the host does not carry a
/// sorting serializer of its own.
fn canonical_json(value: &Value) -> String {
    serde_json_canonicalizer::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tool_limit_tests {
    use super::*;

    #[test]
    fn the_old_reply_count_and_identifier_lengths_are_preserved() {
        assert!(within_tool_limits("id", "lookup", 64));
        assert!(!within_tool_limits("id", "lookup", 65));
        assert!(within_tool_limits(&"i".repeat(256), &"n".repeat(256), 64));
        assert!(!within_tool_limits(&"i".repeat(257), "lookup", 1));
        assert!(!within_tool_limits("id", &"n".repeat(257), 1));
    }
}

#[cfg(test)]
mod decision_tests {
    use super::*;
    use crate::host::{
        json::JsonDoc,
        mind::{Assignment, Assignments, Companion, Profile, State},
        notify::Notifier,
        resources::{DeviceHub, DeviceTimeout, Waiting},
    };
    use rig_core::catalog::{Catalog, Connector, ModelSpec};
    use rig_core::completion::Message;
    use rig_core::conversation::Conversation as History;
    use rig_core::message::{CallId, ToolCall, ToolFunction, ToolName};
    use rig_core::providers::registry::ProviderId;
    use rig_core::test_utils::MockCompletionModel;
    use rig_ecs::session_log::SessionStore;
    use rig_memory::DirStore;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const USER_REQUEST: &str = "デスクトップのスクリーンショットを取って";
    /// Longer than every prompt bound, so a truncation assert can find its tail.
    const HUGE: &str = "huge-path-";
    /// A stall no test waits out: on the paused clock it only has to exceed
    /// [`DECISION_TIMEOUT`].
    const STALL: Duration = Duration::from_secs(600);

    #[test]
    fn the_reply_is_strict_json() {
        let inputs = || DecisionInputs {
            tool_name: "device_request".into(),
            target: "device.files /tmp/a".into(),
            arguments: "{}".into(),
            request: USER_REQUEST.into(),
            policies: Vec::new(),
            rules: Vec::new(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for (reply, allow) in [
            (r#"{"decision":"allow","reason":"安全な操作"}"#, Some(true)),
            (
                r#"{"decision":"require_approval","reason":"外部送信のため確認"}"#,
                Some(false),
            ),
            // An unknown verdict, a missing field, an extra field and plain
            // text are all one thing: not the JSON asked for.
            (r#"{"decision":"maybe","reason":"?"}"#, None),
            (r#"{"decision":"allow"}"#, None),
            (r#"{"decision":"allow","reason":"r","extra":1}"#, None),
            ("はい、いいですよ", None),
        ] {
            let model: DynModel<Completion> = MockCompletionModel::text(reply).into();
            let decision = runtime.block_on(run_decision(model, inputs()));
            match allow {
                Some(allow) => {
                    let decision = decision.expect(reply);
                    assert_eq!(decision.allow, allow, "{reply}");
                    assert!(!decision.reason.is_empty(), "{reply}");
                }
                None => assert!(decision.is_err(), "{reply} must fail safe"),
            }
        }
    }

    #[test]
    fn the_prompt_carries_only_the_bounded_inputs() {
        // Built the way the gate builds them: every string truncated before
        // the inputs exist.
        let inputs = DecisionInputs {
            tool_name: "device_request".into(),
            target: truncate(
                &format!("device.files {}", HUGE.repeat(400)),
                DECISION_ARGUMENT_CHARS,
            ),
            arguments: truncate(
                &serde_json::json!({"path": HUGE.repeat(400)}).to_string(),
                DECISION_ARGUMENT_CHARS,
            ),
            request: truncate(
                &format!("{USER_REQUEST} {}", HUGE.repeat(400)),
                DECISION_REQUEST_CHARS,
            ),
            policies: vec![PolicyText {
                id: "p1".into(),
                text: "外部への送信は常に確認する".into(),
                created_at: String::new(),
            }],
            rules: vec![PolicyRule {
                id: "r1".into(),
                tool_name: "body_motion".into(),
                pattern: "*".into(),
                effect: PolicyEffect::Allow,
                created_at: String::new(),
            }],
        };
        let prompt = decision_prompt(&inputs);
        assert!(prompt.contains("device_request"));
        assert!(prompt.contains(USER_REQUEST));
        assert!(prompt.contains("外部への送信は常に確認する"));
        assert!(prompt.contains("body_motion / * → allow"));
        // Every oversized string is cut at its bound, so the far end of the
        // huge fixture appears in none of them, and the prompt holds the
        // bounded prefix of each.
        let tail = HUGE.repeat(400);
        for bounded in [&inputs.target, &inputs.arguments, &inputs.request] {
            assert!(
                !bounded.contains(&tail[DECISION_ARGUMENT_CHARS..]),
                "the bound must have cut the tail"
            );
        }
        assert!(prompt.contains(&tail[..1_900]));
        assert!(prompt.contains("[保存済みルール（参考）]"));
    }

    #[test]
    fn an_empty_prompt_section_says_so() {
        let prompt = decision_prompt(&DecisionInputs {
            tool_name: "device_request".into(),
            target: "device.files /tmp/a".into(),
            arguments: "{}".into(),
            request: String::new(),
            policies: Vec::new(),
            rules: Vec::new(),
        });
        assert!(prompt.contains("[依頼]\n(なし)"));
        assert!(prompt.contains("[ユーザーの自由文ポリシー]\n(なし)"));
        assert!(prompt.contains("[保存済みルール（参考）]\n(なし)"));
    }

    // === One gated call on a real world ==================================

    /// A local OpenAI-compatible endpoint that answers every completion with
    /// `text` after `delay`, keeping the request bodies it saw. The stalled
    /// variants answer nothing a test waits for (compaction.rs's `serve`).
    struct Provider {
        base: String,
        bodies: Arc<std::sync::Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn serve(text: &'static str, delay: Duration) -> Provider {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&bodies);
        let answer = completion(text);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let body = loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        break None;
                    };
                    if read == 0 {
                        break None;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buffer[..end]);
                    let length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        break Some(String::from_utf8_lossy(&buffer[end + 4..]).to_string());
                    }
                };
                let Some(body) = body else {
                    return;
                };
                seen.lock().unwrap().push(body);
                tokio::time::sleep(delay).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    answer.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(answer.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Provider { base, bodies, task }
    }

    fn completion(text: &str) -> String {
        serde_json::json!({
            "id": "chatcmpl-decision",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string()
    }

    fn paused_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            // The timeout test fires DECISION_TIMEOUT without waiting it out.
            .start_paused(true)
            .build()
            .unwrap()
    }

    /// Advances the world and the paused runtime until every decision lands.
    /// The parked sleep lets the runtime advance the clock to whichever
    /// timer is due: the model reply or the decision timeout.
    fn settle(app: &mut App, runtime: &tokio::runtime::Runtime) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut free = 0;
        while std::time::Instant::now() < deadline {
            runtime.block_on(async {
                tokio::time::sleep(Duration::from_millis(50)).await;
            });
            app.update();
            if app.world().resource::<Decisions>().running.is_empty() {
                free += 1;
                if free >= 2 {
                    return;
                }
            } else {
                free = 0;
            }
        }
        panic!("the decision did not settle in time");
    }

    fn companion(assignments: Assignments) -> Companion {
        let now = crate::host::db::now();
        Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Profile::default(),
            profile_revision: 1,
            state: State::default(),
            state_revision: 1,
            body: JsonDoc::empty(),
            voice: JsonDoc::empty(),
            assignments,
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    /// The Decision role assigned, so the gate asks the model at `provider`.
    fn decision_companion() -> Companion {
        let assignment = Assignment {
            connection_id: "c1".into(),
            model_id: "gpt-4o-mini".into(),
        };
        companion(Assignments {
            task: assignment.clone(),
            decision: Some(assignment),
            ..Default::default()
        })
    }

    /// Only the task model assigned: the Decision role falls back to it.
    fn task_only_companion() -> Companion {
        companion(Assignments {
            task: Assignment {
                connection_id: "c1".into(),
                model_id: "gpt-4o-mini".into(),
            },
            ..Default::default()
        })
    }

    /// The catalog the decision connects through (compaction.rs's `models`).
    fn models() -> Models {
        let provider = ProviderId::catalog("openrouter").unwrap();
        let mut catalog = Catalog::builtin().clone();
        catalog.insert(ModelSpec::new(provider, "gpt-4o-mini").with_context_window(128_000));
        Models(Connector::new(catalog))
    }

    /// The settings row the decision connects through: an OpenRouter adapter
    /// aimed at `endpoint`.
    fn connection_row(endpoint: &str) -> crate::host::settings::Connection {
        use crate::host::settings::{
            ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        };
        crate::host::settings::Connection {
            id: "c1".into(),
            name: "main".into(),
            enabled: true,
            kind: ConnectionKind::Llm,
            config: ConnectionConfig::Llm(LlmConnection {
                endpoint: Endpoint {
                    adapter: "openrouter".into(),
                    endpoint: endpoint.into(),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_ids: vec!["gpt-4o-mini".into()],
            }),
            credential_state: CredentialState::Absent,
            revision: 1,
            last_check: None,
        }
    }

    fn files_spec() -> CapabilitySpec {
        CapabilitySpec {
            name: "device.files".into(),
            description: "files capability".into(),
            input_schema:
                r#"{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}"#
                    .into(),
            approval: CapabilityApproval::Confirm,
            approval_target: "{path}".into(),
            ..Default::default()
        }
    }

    /// One confirm-gated `device.files` client, activated for the companion.
    /// The gate reads clients from the store, so the world needs no client
    /// plugin machinery.
    fn register_files_client(store: &Store) {
        let capability = "device.files".to_owned();
        store
            .register_client_with_grants(
                &crate::host::db::clients::ClientDeclaration {
                    id: "c1",
                    name: "c1",
                    platform: "linux",
                    capabilities: &[capability],
                    specs: std::slice::from_ref(&files_spec()),
                    expected_revision: 0,
                },
                &["device.files".to_owned()],
            )
            .unwrap();
        store.activate("ene", Some("c1")).unwrap();
    }

    /// A headless world around one gated `device_request`: the store, a
    /// client with a confirm-gated `device.files` capability, a companion
    /// whose Decision model answers at `provider`, and the gate and collect
    /// systems. The runtime is the caller's (paused, so a stall costs
    /// nothing); the store lives in `_root`.
    struct DecisionWorld {
        app: App,
        agent: Entity,
        _root: tempfile::TempDir,
    }

    impl DecisionWorld {
        fn at(runtime: &tokio::runtime::Runtime, provider: &str, owner: Companion) -> Self {
            let root = tempfile::tempdir().unwrap();
            let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
            store
                .update_settings(
                    store.settings_revision().unwrap(),
                    &[(connection_row(provider), 0)],
                )
                .unwrap();
            store.insert_companion(&owner).unwrap();
            register_files_client(&store);
            let credentials = Credentials::new(true);
            credentials.remember("c1", "test-key".into());
            // The kernel's start system runs on `Res<SessionLog>`, which the
            // restore of a session log inserts: the gate never runs without
            // it, so the world carries the plugins a real start does.
            let logs = DirStore::open(root.path().join("rig")).unwrap();
            let mut app = App::new();
            app.set_error_handler(bevy_ecs::error::warn)
                .insert_resource(store)
                .insert_resource(credentials)
                .insert_resource(SessionStore::new(logs))
                .insert_resource(Notifier::new())
                .insert_resource(Approvals::default())
                .init_resource::<DeviceHub>()
                .init_resource::<Waiting>()
                .insert_resource(DeviceTimeout(Duration::from_secs(30)))
                .insert_resource(models())
                .insert_resource(crate::host::HostState::default())
                .insert_resource(HostRuntime(runtime.handle().clone()))
                // The gate only sees a call the kernel has a handler for: a
                // `device_request` with no registered tool is dropped before
                // the gate runs, so the client plugin that registers it is
                // part of this world.
                .add_plugins((
                    AgentPlugin,
                    SessionLogPlugin,
                    super::super::client::ClientPlugin,
                    GatePlugin,
                ));
            app.finish();
            if let Some(mut time) = app.world_mut().get_resource_mut::<Time<Virtual>>() {
                time.set_max_delta(Duration::from_secs(60));
            }
            let mut history = History::new();
            history.push(Message::user(USER_REQUEST));
            // `Agent` and `AgentId` are the kernel's own identity for an
            // agent: without them the start stage finds no agent for the call
            // and drops it, so the gate never runs.
            let agent = app
                .world_mut()
                .spawn((
                    Agent,
                    AgentId(crate::host::agent::companion_agent_id("ene")),
                    super::super::agent::CompanionAgent {
                        companion_id: "ene".into(),
                    },
                    Conversation(history),
                ))
                .id();
            Self {
                app,
                agent,
                _root: root,
            }
        }

        fn store(&self) -> Store {
            self.app.world().resource::<Store>().clone()
        }

        /// Spawns one gated `device_request` call, the way a model reply does.
        fn gated_call(&mut self, path: &str) -> Entity {
            let call = ToolCall {
                id: CallId::from_wire("call-1"),
                function: ToolFunction::new(
                    ToolName::new("device_request").unwrap(),
                    serde_json::json!({"capability": "device.files", "input": {"path": path}}),
                ),
                native: None,
            };
            self.app
                .world_mut()
                .spawn((
                    rig_ecs::tools::ToolRequest { call },
                    rig_ecs::stage::Effect::default(),
                    rig_ecs::stage::ForAgent(self.agent),
                    rig_ecs::stage::Admitted,
                ))
                .id()
        }

        /// The one pending approval, once the gate has held the call.
        fn pending(&self) -> Approval {
            self.store()
                .approvals(ApprovalStatus::Pending)
                .unwrap()
                .pop()
                .expect("one held approval")
        }

        fn events(&mut self) -> Vec<w::Notification> {
            let mut receiver = self.app.world().resource::<Notifier>().subscribe();
            self.app.world_mut().resource_mut::<Notifier>().flush();
            let mut seen = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                seen.push(event);
            }
            seen
        }
    }

    /// What one notification stream says about cards and resolutions.
    fn cards(events: &[w::Notification]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                let request = match event.event.as_ref()? {
                    w::notification::Event::ApprovalRequested(request) => request,
                    _ => return None,
                };
                request
                    .request
                    .as_option()
                    .map(|request| request.request_id.clone())
            })
            .collect()
    }

    fn notices(events: &[w::Notification], code: &str) -> usize {
        events
            .iter()
            .filter(|event| {
                matches!(
                    event.event.as_ref(),
                    Some(w::notification::Event::Generic(inner))
                        if inner.kind == "notice"
                            && inner.payload.as_option().is_some_and(|payload| {
                                payload.json.contains(code)
                            })
                )
            })
            .count()
    }

    #[test]
    fn a_safe_operation_the_model_allows_runs_without_a_card() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"allow","reason":"依頼どおりの安全な操作です"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        assert!(
            world.app.world().resource::<Decisions>().running.len() == 1,
            "the gate started one decision"
        );
        settle(&mut world.app, &runtime);

        let events = world.events();
        assert!(cards(&events).is_empty(), "no card: {events:?}");
        assert_eq!(notices(&events, "approval_resolved"), 1);
        let approval = world.store().approval(&request).unwrap().unwrap();
        assert_eq!(approval.status, ApprovalStatus::Approved);
        assert!(world.app.world().get::<Approved>(call).is_some());
        assert!(
            world
                .app
                .world()
                .resource::<Approvals>()
                .by_request
                .is_empty(),
            "the hold is released"
        );
        assert_eq!(
            world
                .store()
                .approvals(ApprovalStatus::Pending)
                .unwrap()
                .len(),
            0
        );
        let bodies = provider.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1, "one decision call");
        provider.task.abort();
    }

    #[test]
    fn an_operation_that_breaks_a_free_text_policy_is_carded_once() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"require_approval","reason":"ポリシー: 外部への送信は常に確認"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        world
            .store()
            .apply_policy_texts(
                &[PolicyText {
                    id: "p1".into(),
                    text: "外部への送信は常に確認する".into(),
                    created_at: String::new(),
                }],
                &[],
            )
            .unwrap();
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        settle(&mut world.app, &runtime);

        let events = world.events();
        assert_eq!(cards(&events), vec![request.clone()], "exactly one card");
        assert_eq!(notices(&events, "approval_required"), 1);
        let approval = world.store().approval(&request).unwrap().unwrap();
        assert_eq!(approval.status, ApprovalStatus::Pending);
        assert!(world.app.world().get::<Approved>(call).is_none());
        assert_eq!(
            world.app.world().resource::<Approvals>().by_request.len(),
            1,
            "the call stays held"
        );
        // The card the user answers is the real one: submission works.
        let resolved = submit(world.app.world_mut(), &request, ApprovalDecision::Deny).unwrap();
        assert_eq!(resolved.status, ApprovalStatus::Denied);
        provider.task.abort();
    }

    #[test]
    fn a_timeout_or_an_invalid_reply_fails_safe_to_the_card() {
        let runtime = paused_runtime();
        // The endpoint stalls past the deadline: only the timeout answers.
        let provider =
            runtime.block_on(serve(r#"{"decision":"allow","reason":"遅すぎる"}"#, STALL));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        settle(&mut world.app, &runtime);
        let events = world.events();
        assert_eq!(cards(&events), vec![request.clone()], "the timeout cards");
        assert!(
            world.store().approval(&request).unwrap().unwrap().status == ApprovalStatus::Pending
        );
        assert!(world.app.world().get::<Approved>(call).is_none());
        assert!(provider.bodies.lock().unwrap().len() <= 1);
        provider.task.abort();

        // The reply is prose, not the JSON asked for.
        let provider = runtime.block_on(serve("はい、承認します", Duration::ZERO));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        settle(&mut world.app, &runtime);
        let events = world.events();
        assert_eq!(
            cards(&events),
            vec![request.clone()],
            "the invalid reply cards"
        );
        assert!(
            world.store().approval(&request).unwrap().unwrap().status == ApprovalStatus::Pending
        );
        assert!(world.app.world().get::<Approved>(call).is_none());
        provider.task.abort();
    }

    #[test]
    fn the_prompt_the_model_received_carries_the_bounded_inputs() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"require_approval","reason":"対象が大きすぎる"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        world
            .store()
            .apply_policy_texts(
                &[PolicyText {
                    id: "p1".into(),
                    text: "外部への送信は常に確認する".into(),
                    created_at: String::new(),
                }],
                &[],
            )
            .unwrap();
        // A rule for another tool reaches the prompt as reference, without
        // deciding the call.
        world
            .store()
            .apply_policy_rules(
                &[PolicyRule {
                    id: "r1".into(),
                    tool_name: "body_motion".into(),
                    pattern: "*".into(),
                    effect: PolicyEffect::Allow,
                    created_at: String::new(),
                }],
                &[],
            )
            .unwrap();
        let _ = world.gated_call("/tmp/a");
        world.app.update();
        settle(&mut world.app, &runtime);

        let bodies = provider.bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1, "one decision call");
        let body = &bodies[0];
        assert!(body.contains("device_request"), "{body}");
        assert!(body.contains(USER_REQUEST), "{body}");
        assert!(body.contains("外部への送信は常に確認する"), "{body}");
        assert!(body.contains("body_motion / * → allow"), "{body}");
        // The canonical arguments reach the prompt; the body embeds it as a
        // JSON string, so its quotes travel escaped.
        let arguments = r#"{"capability":"device.files","input":{"path":"/tmp/a"}}"#;
        assert!(
            body.contains(&arguments.replace('"', "\\\"")),
            "the canonical arguments: {body}"
        );
        assert!(
            body.contains("require_approval"),
            "the system prompt demands the JSON shape: {body}"
        );
        provider.task.abort();
    }

    #[test]
    fn a_late_decision_drops_when_the_user_answered_first() {
        let runtime = paused_runtime();
        // The model allows, but only after the user already denied.
        let provider = runtime.block_on(serve(
            r#"{"decision":"allow","reason":"安全だった"}"#,
            Duration::from_secs(1),
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        let denied = submit(world.app.world_mut(), &request, ApprovalDecision::Deny).unwrap();
        assert_eq!(denied.status, ApprovalStatus::Denied);
        settle(&mut world.app, &runtime);

        let events = world.events();
        assert!(cards(&events).is_empty(), "a late allow shows no card");
        assert!(
            world.store().approval(&request).unwrap().unwrap().status == ApprovalStatus::Denied,
            "the deny stands"
        );
        assert!(
            world
                .store()
                .resolve_approval(&request, ApprovalStatus::Approved)
                .is_err(),
            "the approval is resolved once"
        );
        assert!(world.app.world().get::<Approved>(call).is_none());
        assert!(world.app.world().resource::<Decisions>().running.is_empty());
        provider.task.abort();
    }

    #[test]
    fn the_decision_role_falls_back_to_the_task_model() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"allow","reason":"依頼どおりの操作"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, task_only_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        settle(&mut world.app, &runtime);

        assert_eq!(
            provider.bodies.lock().unwrap().len(),
            1,
            "the task model answered"
        );
        let approval = world.store().approval(&request).unwrap().unwrap();
        assert_eq!(approval.status, ApprovalStatus::Approved);
        assert!(world.app.world().get::<Approved>(call).is_some());
        provider.task.abort();
    }

    #[test]
    fn always_approve_persists_allow_rule_and_skips_second_prompt() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"require_approval","reason":"外部ファイル操作"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call1 = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        let approved = submit(
            world.app.world_mut(),
            &request,
            ApprovalDecision::AlwaysApprove,
        )
        .unwrap();
        assert_eq!(approved.status, ApprovalStatus::Approved);
        assert!(world.app.world().get::<Approved>(call1).is_some());

        let rules = world.store().policy_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effect, PolicyEffect::Allow);

        // A second matching call is auto-approved directly from the saved rule
        // without asking the decision model or raising a second approval card.
        let calls_before = provider.bodies.lock().unwrap().len();
        let call2 = world.gated_call("/tmp/a");
        world.app.update();
        assert!(
            world
                .app
                .world()
                .resource::<Waiting>()
                .by_operation
                .values()
                .any(|wait| wait.call == call2)
        );
        assert!(
            world
                .store()
                .approvals(ApprovalStatus::Pending)
                .unwrap()
                .is_empty()
        );
        assert_eq!(provider.bodies.lock().unwrap().len(), calls_before);
        world.gated_call("/tmp/b");
        world.app.update();
        assert_eq!(
            world
                .store()
                .approvals(ApprovalStatus::Pending)
                .unwrap()
                .len(),
            1
        );
        provider.task.abort();
    }

    #[test]
    fn b20_confirm_rule_is_never_bypassed_by_decision_allow() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"allow","reason":"safe"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        world
            .store()
            .apply_policy_rules(
                &[PolicyRule {
                    id: "confirm-x".into(),
                    tool_name: "device.files".into(),
                    pattern: "/tmp/x".into(),
                    effect: PolicyEffect::Confirm,
                    created_at: String::new(),
                }],
                &[],
            )
            .unwrap();
        let call = world.gated_call("/tmp/x");
        world.app.update();
        assert!(
            world.app.world().resource::<Decisions>().running.is_empty(),
            "HumanRequired must skip Decision LLM"
        );
        let request = world.pending().request_id;
        let events = world.events();
        assert_eq!(cards(&events), vec![request.clone()]);
        assert!(world.app.world().get::<Approved>(call).is_none());
        assert_eq!(provider.bodies.lock().unwrap().len(), 0);
        let resolved = submit(world.app.world_mut(), &request, ApprovalDecision::Approve).unwrap();
        assert_eq!(resolved.status, ApprovalStatus::Approved);
        assert!(world.app.world().get::<Approved>(call).is_some());
        provider.task.abort();
    }

    #[test]
    fn b06_submit_keeps_hold_when_bound_client_loses_grant() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"require_approval","reason":"ask"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let call = world.gated_call("/tmp/a");
        world.app.update();
        let request = world.pending().request_id;
        let mut client = world.store().client("c1").unwrap().unwrap();
        client.granted_capabilities.clear();
        world.store().restore_client(&client).unwrap();

        let err = submit(world.app.world_mut(), &request, ApprovalDecision::Approve).unwrap_err();
        assert!(matches!(err, HostError::Conflict(_)));
        assert_eq!(
            world.store().approval(&request).unwrap().unwrap().status,
            ApprovalStatus::Pending
        );
        assert!(
            world
                .app
                .world()
                .resource::<Approvals>()
                .by_request
                .contains_key(&request),
            "hold remains intact after pre-commit failure"
        );
        assert!(world.app.world().get::<Approved>(call).is_none());
        provider.task.abort();
    }

    #[test]
    fn b18_large_always_approve_persists_exact_digest_rule_and_skips_second_prompt() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"require_approval","reason":"large payload"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        let big_path = format!("/tmp/{}", "a".repeat(4200));
        let call1 = world.gated_call(&big_path);
        world.app.update();
        let request = world.pending().request_id;
        let outcome = submit_with_outcome(
            world.app.world_mut(),
            &request,
            ApprovalDecision::AlwaysApprove,
        )
        .unwrap();
        assert!(outcome.rule_saved, "{:?}", outcome.rule_error);
        assert!(world.app.world().get::<Approved>(call1).is_some());

        let rules = world.store().policy_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert!(rules[0].pattern.starts_with("exact_sha256:"));

        let call2 = world.gated_call(&big_path);
        world.app.update();
        assert!(
            world
                .app
                .world()
                .resource::<Waiting>()
                .by_operation
                .values()
                .any(|wait| wait.call == call2)
        );
        assert!(
            world
                .store()
                .approvals(ApprovalStatus::Pending)
                .unwrap()
                .is_empty()
        );
        provider.task.abort();
    }

    #[test]
    fn b05_policy_read_failure_fails_closed_instead_of_treating_as_empty() {
        let runtime = paused_runtime();
        let provider = runtime.block_on(serve(
            r#"{"decision":"allow","reason":"safe"}"#,
            Duration::ZERO,
        ));
        let mut world = DecisionWorld::at(&runtime, &provider.base, decision_companion());
        // Corrupt policy_rules row triggers HostError::Storage on policy_rules().
        world
            .store()
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO policy_rules (id, tool_name, pattern, effect, created_at) \
                 VALUES ('bad', '*', '*', 'bogus', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        let call = world.gated_call("/tmp/a");
        world.app.update();
        assert!(
            world
                .app
                .world()
                .resource::<crate::host::HostState>()
                .failed
                .is_some(),
            "corrupted policy rules must fail the host instead of returning empty rules"
        );
        assert!(world.app.world().get::<Approved>(call).is_none());
        provider.task.abort();
    }
}
