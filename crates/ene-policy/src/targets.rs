//! Typed action targets and the pattern matching a saved rule applies to them.
use crate::capability::COMPUTER_USE;
use serde_json::Value;

use crate::rules::CandidateAction;

/// Typed target extracted from a `CandidateAction` at the permission boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionTarget {
    /// A filesystem path, normalised to forward slashes.
    Path(String),
    /// A shell command the action would run.
    Command(String),
    /// A URL, with its lowercased host when one parsed.
    Url {
        /// The URL as the action supplied it.
        url: String,
        /// The parsed host, lowercased, when the URL had one.
        host: Option<String>,
    },
    /// A free-form subject matched as a pattern, such as a motion name.
    Pattern(String),
    /// Nothing addressable was found in the arguments.
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

    pub(super) fn matches_pattern(&self, pattern: &str) -> bool {
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

/// Reads the typed target out of an action's arguments, for rule matching.
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
        CandidateAction::WebSearch(search) => ActionTarget::Url {
            url: search.endpoint.as_str().to_owned(),
            host: search.endpoint.host_str().map(str::to_ascii_lowercase),
        },
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
