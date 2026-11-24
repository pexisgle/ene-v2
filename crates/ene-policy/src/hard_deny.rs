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
