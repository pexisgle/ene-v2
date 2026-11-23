//! Saved policy rules, free-text policies, canonical hashing and gate evaluation.
use crate::capability::{
    BUILTIN_TOOLS, COMPUTER_USE, CapabilityApproval, CapabilitySpec, approval_target, truncate,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use strum::{EnumString, IntoStaticStr};

const WEB_SEARCH_TOOL: &str = "web_search";

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

/// Matches a policy pattern against one typed target. `*` and `**` are the only
/// wildcards; every other glob metacharacter is classed so it stays literal.
fn wildcard_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value;
    }
    // Classes, not backslashes: `\` is a path separator on Windows. Only a `*`
    // pattern reaches here, so the ~60µs glob compile stays off the common path.
    let mut escaped = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '[' => escaped.push_str("[[]"),
            ']' => escaped.push_str("[]]"),
            '{' => escaped.push_str("[{]"),
            '}' => escaped.push_str("[}]"),
            '?' => escaped.push_str("[?]"),
            _ => escaped.push(c),
        }
    }
    // Escaping leaves nothing that can fail to compile, but a rule that cannot
    // must still match literally rather than silently stop matching.
    globset::GlobBuilder::new(&escaped)
        .backslash_escape(false)
        .build()
        .map_or_else(
            |_| pattern == value,
            |glob| glob.compile_matcher().is_match(value),
        )
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
/// How much damage an approval's action could do, for the UI to show.
///
/// It is derived from the same capability and tool-name rules that decide the
/// hard-deny and mutation cases, so the badge a person sees cannot disagree
/// with what the policy engine actually blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Reads, searches, and other actions that change nothing.
    Low,
    /// Writes a file or creates an external effect.
    Moderate,
    /// Deletes, or runs a shell command the user cannot preview.
    High,
}

impl RiskLevel {
    /// Risk of `capability` (or `tool_name` when there is no capability) acting
    /// on `arguments`, which are the tool's normalised JSON arguments.
    ///
    /// Anything hard-denied is `High` without inspecting further: a person
    /// looking at the card needs to know the host blocks it regardless.
    pub fn of(capability: Option<&str>, tool_name: &str, arguments: &str) -> Self {
        let subject = capability
            .filter(|value| !value.is_empty())
            .unwrap_or(tool_name);
        if hard_denied(subject).is_some() {
            return Self::High;
        }
        let lower_tool = tool_name.to_ascii_lowercase();
        // A delete, move-over, or shell command is High: the first destroys,
        // the second can overwrite a file the person did not name, and the
        // third does something the card cannot summarise.
        if subject.ends_with(".delete")
            || subject.ends_with(".shell")
            || subject == "device.shell"
            || subject == "computer.use"
            || lower_tool.contains("delete")
            || lower_tool.contains("remove")
            || lower_tool.contains("shell")
            || lower_tool.contains("exec")
        {
            return Self::High;
        }
        // Reading is harmless even though it names a path, so the read-only
        // spelling is decided before the argument heuristic below.
        let reads_only = (subject.ends_with(".read")
            || subject.ends_with(".list")
            || subject.ends_with(".search")
            || lower_tool.contains("read")
            || lower_tool.contains("list")
            || lower_tool.contains("search")
            || lower_tool.contains("get"))
            && !lower_tool.contains("write")
            && !lower_tool.contains("edit")
            && !lower_tool.contains("delete")
            && !lower_tool.contains("remove");
        if reads_only {
            return Self::Low;
        }
        if is_file_mutation_subject(subject)
            || lower_tool.contains("write")
            || lower_tool.contains("edit")
        {
            return Self::Moderate;
        }
        // An MCP or plugin tool whose arguments name a path can write it.
        if arguments.contains("\"path\"") || arguments.contains("\"file\"") {
            return Self::Moderate;
        }
        Self::Low
    }
}

/// The capability spellings that mutate a file, matching
/// `is_file_mutation_action` so the badge and the policy agree.
fn is_file_mutation_subject(subject: &str) -> bool {
    matches!(
        subject,
        "work.write" | "work.edit" | "work.delete" | "device.files"
    ) || subject.starts_with("work.file")
}

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

// Kept beside `wildcard_matches` because the function is private to this module.
#[cfg(test)]
mod wildcard_tests {
    use super::*;

    /// The pattern shapes a saved rule can carry. `*` stays all-match, a leading
    /// or trailing `*` keeps its anchoring, `**` is the one deliberate widening
    /// (it used to fail for any non-empty value), and glob metacharacters stay
    /// literal rather than becoming a class or alternation.
    #[test]
    fn wildcard_patterns_keep_their_meaning_under_glob_matching() {
        let cases: &[(&str, &str, bool)] = &[
            ("*", "/tmp/anything", true),
            ("*", "", true),
            ("prefix*", "prefix-and-more", true),
            ("prefix*", "prefi", false),
            ("*suffix", "elsewhere-suffix", true),
            ("*suffix", "suffi", false),
            ("foo*bar", "foo-mid-bar", true),
            ("foo*bar", "foobar-then-more", false),
            ("**", "anything", true),
            ("**", "", true),
            ("literal", "literal", true),
            ("literal", "litera", false),
            // Not `?`-any, not a `[x]` class, not a `{a,b}` alternation, not a `]`.
            ("/tmp/[x]/f", "/tmp/[x]/f", true),
            ("/tmp/[x]/f", "/tmp/x/f", false),
            ("a?b", "a?b", true),
            ("a?b", "axb", false),
            ("{a,b}", "{a,b}", true),
            ("{a,b}", "a", false),
            ("a]b", "a]b", true),
        ];
        for (pattern, value, expected) in cases {
            assert_eq!(
                wildcard_matches(pattern, value),
                *expected,
                "pattern {pattern:?} against {value:?}"
            );
        }
    }
}
