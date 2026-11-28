use super::{FILE_LIMIT, WorkToolError};
use globset::{GlobBuilder, GlobMatcher};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Component, Path};

pub(super) fn relative(path: &str) -> Result<&Path, WorkToolError> {
    let parsed = Path::new(path);
    if path.is_empty()
        || (cfg!(windows) && path.contains(':'))
        || parsed
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(WorkToolError::InvalidPath);
    }
    Ok(parsed)
}

pub(super) fn text(bytes: &[u8]) -> Result<&str, WorkToolError> {
    if bytes.contains(&0) {
        return Err(WorkToolError::BinaryFile);
    }
    std::str::from_utf8(bytes).map_err(|_| WorkToolError::NotUtf8)
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Edit {
    #[schemars(length(min = 1))]
    old_text: String,
    new_text: String,
}

pub(super) fn edit(content: &str, edits: &[Edit]) -> Result<String, WorkToolError> {
    if edits.is_empty() {
        return Err(WorkToolError::EmptyEdits);
    }
    let mut content = content.to_owned();
    for Edit {
        old_text: old,
        new_text: new,
    } in edits
    {
        let Some(first_char) = old.chars().next() else {
            return Err(WorkToolError::EmptyOldText);
        };
        let Some(index) = content.find(old) else {
            return Err(WorkToolError::OldTextNotFound);
        };
        let end = index + old.len();
        // Check overlapping occurrences as well.
        let next = index + first_char.len_utf8();
        if content[next..].contains(old) {
            return Err(WorkToolError::OldTextNotUnique);
        }
        if content.len() - old.len() + new.len() > FILE_LIMIT {
            return Err(WorkToolError::FileTooLarge);
        }
        content.replace_range(index..end, new);
    }
    Ok(content)
}

pub(super) fn glob(pattern: &str) -> Result<(GlobMatcher, bool), WorkToolError> {
    Ok((
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()?
            .compile_matcher(),
        pattern.contains('/'),
    ))
}
