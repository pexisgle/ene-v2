//! Host hard denials: capability prefixes, system paths and destructive
//! commands, plus host-tool classification.
use crate::capability::BUILTIN_TOOLS;

use crate::rules::{CandidateAction, WEB_SEARCH_TOOL};
use crate::targets::ActionTarget;

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

/// Returns the matching hard-denied prefix, when the capability starts with one.
pub fn hard_denied(capability: &str) -> Option<&'static str> {
    HARD_DENIED
        .iter()
        .find(|prefix| capability.starts_with(**prefix))
        .copied()
}

/// The reason the host blocks this action outright, bypassing saved rules.
///
/// A `None` means the action is still subject to the rule engine; it does not
/// mean the action is allowed.
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
    if matches!(action, CandidateAction::WebSearch(_))
        && let ActionTarget::Url { url, host } = target
        && hard_denied_search_destination(url, host.as_deref())
    {
        return Some("the web search destination is hard-blocked".into());
    }
    None
}

/// Route restrictions shared by ordinary searches and explicit connection checks.
pub fn hard_denied_search_destination(url: &str, host: Option<&str>) -> bool {
    url.get(..7)
        .is_some_and(|start| start.eq_ignore_ascii_case("file://"))
        || host.is_some_and(|host| {
            host.trim_end_matches('.')
                .eq_ignore_ascii_case("metadata.google.internal")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| {
                        ip.to_canonical() == std::net::Ipv4Addr::new(169, 254, 169, 254)
                    })
        })
}

fn is_file_mutation_action(action: &CandidateAction) -> bool {
    match action {
        CandidateAction::DeviceCapability { capability, .. } => matches!(
            capability.as_str(),
            "work.write" | "work.edit" | "work.delete" | "device.files"
        ),
        CandidateAction::ExternalTool(tool) => mutating_tool_name(&tool.tool_name),
        CandidateAction::WebSearch(_) => false,
    }
}

fn mutating_tool_name(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    ["write", "edit", "delete", "remove"]
        .iter()
        .any(|name| lower.contains(name))
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
