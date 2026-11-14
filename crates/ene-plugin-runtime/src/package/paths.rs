//! Portable package paths and identifiers.
use crate::{Error, Result};
use std::collections::BTreeMap;

/// Portable package paths use forward slashes, never native device paths.
pub(super) fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(':')
                && !part.contains(['<', '>', '"', '|', '?', '*'])
                && !part.ends_with(['.', ' '])
                && !matches!(
                    part.split('.')
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches(' ')
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                        | "CONIN$"
                        | "CONOUT$"
                        | "COM¹"
                        | "COM²"
                        | "COM³"
                        | "LPT¹"
                        | "LPT²"
                        | "LPT³"
                )
        })
}
pub fn identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
pub(super) fn portable_name(
    names: &mut BTreeMap<String, (String, bool)>,
    path: &str,
) -> Result<()> {
    if !relative(path) {
        return Err(Error::Invalid);
    }
    let mut prefix = String::new();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let directory = parts.peek().is_some();
        match names.entry(prefix.to_lowercase()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((prefix.clone(), directory));
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if directory && entry.get() == &(prefix.clone(), true) => {}
            _ => return Err(Error::Invalid),
        }
    }
    Ok(())
}
