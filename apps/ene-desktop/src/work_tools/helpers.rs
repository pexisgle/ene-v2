use super::{FILE_LIMIT, WorkToolError};
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
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

pub(super) fn required<'a>(input: &'a Value, name: &'static str) -> Result<&'a str, WorkToolError> {
    input[name].as_str().ok_or(WorkToolError::NotString(name))
}

pub(super) fn positive(
    input: &Value,
    name: &'static str,
    default: usize,
) -> Result<usize, WorkToolError> {
    match input.get(name) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|&value| value > 0)
            .ok_or(WorkToolError::NotPositiveInt(name)),
    }
}

pub(super) fn text(bytes: &[u8]) -> Result<&str, WorkToolError> {
    if bytes.contains(&0) {
        return Err(WorkToolError::BinaryFile);
    }
    std::str::from_utf8(bytes).map_err(|_| WorkToolError::NotUtf8)
}

/// The formats the read tool hands to the model as a base64 image.
///
/// `guess_format` recognises more than this (BMP, TIFF, ICO, ...), but the
/// downstream image pipeline only ever received these four, so the set stays
/// explicit rather than silently widening what is sent.
const IMAGE_FORMATS: [image::ImageFormat; 4] = [
    image::ImageFormat::Png,
    image::ImageFormat::Jpeg,
    image::ImageFormat::Gif,
    image::ImageFormat::WebP,
];

pub(super) fn image(bytes: &[u8]) -> bool {
    image::guess_format(bytes).is_ok_and(|format| IMAGE_FORMATS.contains(&format))
}

pub(super) fn edit(content: &str, input: &Value) -> Result<String, WorkToolError> {
    let edits = input["edits"]
        .as_array()
        .filter(|edits| !edits.is_empty())
        .ok_or(WorkToolError::EmptyEdits)?;
    let mut content = content.to_owned();
    for edit in edits {
        let old = required(edit, "oldText")?;
        let new = required(edit, "newText")?;
        if old.is_empty() {
            return Err(WorkToolError::EmptyOldText);
        }
        let Some(index) = content.find(old) else {
            return Err(WorkToolError::OldTextNotFound);
        };
        let end = index + old.len();
        // Check overlapping occurrences as well.
        let Some(first_char) = old.chars().next() else {
            return Err(WorkToolError::EmptyOldText);
        };
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
