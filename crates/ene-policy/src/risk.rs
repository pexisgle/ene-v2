//! Risk badge shown on an approval card, derived from the policy rules.
use crate::hard_deny::hard_denied;

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
