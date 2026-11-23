//! Saved policy rules, free-text policies, canonical hashing and gate evaluation.
use crate::capability::{
    BUILTIN_TOOLS, COMPUTER_USE, CapabilityApproval, CapabilitySpec, approval_target, truncate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const WEB_SEARCH_TOOL: &str = "web_search";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    Allow,
    Deny,
    Confirm,
    AskLlm,
}

/// The spelling stored in policy rules.
impl PolicyEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Confirm => "confirm",
            Self::AskLlm => "ask_llm",
        }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalStatus {
    pub fn as_str(self) -> &'static str {
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

/// Typed target extracted from a `CandidateAction` at the permission boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionTarget {
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

pub fn extract_target(action: &CandidateAction) -> ActionTarget {
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
            if capability == COMPUTER_USE {
                return ActionTarget::None;
            }
            extract_common_target(input)
        }
        CandidateAction::McpTool { input, .. } | CandidateAction::PluginTool { input, .. } => {
            extract_common_target_with_dir(input)
        }
        CandidateAction::WebSearch { endpoint, .. } => ActionTarget::from_url(endpoint),
    }
}

fn extract_common_target(input: &Value) -> ActionTarget {
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

fn extract_common_target_with_dir(input: &Value) -> ActionTarget {
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
pub fn is_host_tool(tool_name: &str) -> bool {
    !matches!(tool_name, "device_request" | WEB_SEARCH_TOOL) && BUILTIN_TOOLS.contains(&tool_name)
}

pub fn hard_denied(capability: &str) -> Option<&'static str> {
    HARD_DENIED
        .iter()
        .find(|prefix| capability.starts_with(**prefix))
        .copied()
}

pub fn hard_denied_action(action: &CandidateAction, target: &ActionTarget) -> Option<String> {
    if let CandidateAction::DeviceCapability { capability, .. } = action
        && let Some(prefix) = hard_denied(capability)
    {
        return Some(format!(
            "`{capability}` is hard-blocked by the host ({prefix}*)"
        ));
    }
    if is_file_mutation_action(action)
        && let ActionTarget::Path(path) = target
        && let Some(reason) = denied_system_path(path)
    {
        return Some(reason);
    }
    if let ActionTarget::Command(cmd) = target
        && let Some(reason) = denied_command(cmd)
    {
        return Some(reason);
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

fn is_file_mutation_action(action: &CandidateAction) -> bool {
    match action {
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
    }
}

fn denied_system_path(path: &str) -> Option<String> {
    let lower = path.to_ascii_lowercase();
    for forbidden in SYSTEM_MUTATION_PATHS {
        if lower == *forbidden || lower.starts_with(forbidden) {
            return Some(format!(
                "system path `{path}` is hard-blocked from modification"
            ));
        }
    }
    None
}

fn denied_command(cmd: &str) -> Option<String> {
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
    None
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
        let ask_llm = rule(PolicyEffect::AskLlm, "device_request", "/tmp/ask");
        let ask_over_allow = decide_capability(
            &[allow.clone(), ask_llm.clone()],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/ask"}}),
        );
        assert!(matches!(ask_over_allow, Verdict::NeedsDecision { .. }));
        let confirm_over_ask = decide_capability(
            &[
                ask_llm,
                rule(PolicyEffect::Confirm, "device_request", "/tmp/ask"),
            ],
            Some(&spec(CapabilityApproval::Auto)),
            "device.files",
            "device_request",
            &json!({"capability":"device.files","input":{"path": "/tmp/ask"}}),
        );
        assert!(matches!(confirm_over_ask, Verdict::HumanRequired { .. }));
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

#[cfg(test)]
mod canonical_property_tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    /// A JSON value of arbitrary shape, including nested objects and arrays.
    fn any_json() -> impl Strategy<Value = serde_json::Value> {
        let leaf = prop_oneof![
            any::<i64>().prop_map(|n| json!(n)),
            any::<bool>().prop_map(|b| json!(b)),
            "[a-z]{0,8}".prop_map(|s| json!(s)),
            Just(serde_json::Value::Null),
        ];
        leaf.prop_recursive(4, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(|v| json!(v)),
                prop::collection::hash_map("[a-z]{1,5}", inner, 0..4).prop_map(|m| json!(m)),
            ]
        })
    }

    proptest! {
        /// The same value always canonicalises to the same bytes. The gate
        /// compares digests, so a non-deterministic form would make a saved
        /// allow rule stop matching the call it was saved for.
        #[test]
        fn canonicalisation_is_deterministic(value in any_json(), tool in "[a-z]{1,8}") {
            let (first_text, first_hash) = canonicalise(&tool, &value);
            let (second_text, second_hash) = canonicalise(&tool, &value);
            prop_assert_eq!(first_text, second_text);
            prop_assert_eq!(first_hash, second_hash);
        }

        /// Key insertion order must not matter. Two objects with the same
        /// members in a different order are the same call, and must produce
        /// the same digest.
        #[test]
        fn key_order_does_not_change_the_digest(
            a in "[a-z]{1,4}",
            b in "[a-z]{1,4}",
            x in any::<i64>(),
            y in any::<i64>(),
        ) {
            prop_assume!(a != b);
            let mut first = serde_json::Map::new();
            first.insert(a.clone(), json!(x));
            first.insert(b.clone(), json!(y));
            let mut second = serde_json::Map::new();
            second.insert(b, json!(y));
            second.insert(a, json!(x));
            let (_, first_hash) = canonicalise("t", &serde_json::Value::Object(first));
            let (_, second_hash) = canonicalise("t", &serde_json::Value::Object(second));
            prop_assert_eq!(first_hash, second_hash);
        }

        /// The tool name is part of the identity: the same arguments under a
        /// different tool must not collide, or an approval for one tool would
        /// authorise another.
        #[test]
        fn the_tool_name_separates_otherwise_equal_calls(
            value in any_json(),
            left in "[a-z]{1,6}",
            right in "[a-z]{1,6}",
        ) {
            prop_assume!(left != right);
            let (_, first) = canonicalise(&left, &value);
            let (_, second) = canonicalise(&right, &value);
            prop_assert_ne!(first, second);
        }

        /// A hex digest is lowercase hex of exactly twice the input length.
        #[test]
        fn hex_digests_have_a_fixed_shape(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
            let text = hex_digest(&bytes);
            prop_assert_eq!(text.len(), bytes.len() * 2);
            prop_assert!(text.chars().all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        }

        /// The digest feeds the approval payload hash, so its bytes are pinned:
        /// a different encoding would silently invalidate every stored
        /// `exact_sha256:` approval.
        #[test]
        fn hex_digests_match_the_pinned_encoding(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
            let expected: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            prop_assert_eq!(hex_digest(&bytes), expected);
        }
    }
}
