//! Bounded search through open workspace directory capabilities.

use super::{
    GREP_BYTES, MAX_RESULTS, OUTPUT_LIMIT, SCAN_LIMIT, WorkToolError,
    helpers::{relative, text},
    require_active,
    workspace::Workspace,
};
use cap_fs_ext::DirExt as _;
use cap_std::fs::{Dir, FileType};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io, num::NonZeroUsize, path::Path};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
pub(super) enum Search<'a> {
    List,
    Find(&'a (globset::GlobMatcher, bool)),
    Grep {
        regex: &'a regex::Regex,
        glob: Option<&'a (globset::GlobMatcher, bool)>,
    },
}

#[derive(Deserialize, JsonSchema)]
pub(super) struct Browse {
    /// Path relative to the workspace, using / between components.
    #[serde(default = "workspace_root")]
    path: String,
    #[serde(default = "max_results")]
    #[schemars(range(max = MAX_RESULTS))]
    limit: NonZeroUsize,
}

fn workspace_root() -> String {
    ".".into()
}
pub(super) fn max_results() -> NonZeroUsize {
    const { NonZeroUsize::new(MAX_RESULTS).expect("the result limit is positive") }
}

impl Workspace {
    pub(super) fn search(
        &self,
        browse: &Browse,
        search: Search<'_>,
        stop: &CancellationToken,
    ) -> Result<Value, WorkToolError> {
        let path = relative(&browse.path)?;
        let limit = browse.limit.get().min(MAX_RESULTS);
        let mut entries = Vec::new();
        let mut remaining_bytes = GREP_BYTES;
        let mut output_bytes = 0;
        let mut scanned = 0;
        let mut clipped_line = false;
        let mut skipped = 0;
        let glob = match search {
            Search::List => None,
            Search::Find(glob) => Some(glob),
            Search::Grep { glob, .. } => glob,
        };
        let mut push = |path: &Path, directory: bool, matched: Option<(usize, &str)>| {
            output_bytes += path.as_os_str().len() + matched.map_or(0, |(_, line)| line.len());
            if entries.len() == limit || output_bytes > OUTPUT_LIMIT {
                return false;
            }
            let path = path.to_string_lossy();
            entries.push(match matched {
                Some((line, text)) => json!({"path": path, "line": line, "text": text}),
                None => json!({"path": path, "directory": directory}),
            });
            true
        };
        let mut visit = |entry: &Path, kind: FileType| -> bool {
            scanned += 1;
            if scanned > SCAN_LIMIT || stop.is_cancelled() {
                return false;
            }
            if glob.is_some_and(|(glob, nested)| {
                let candidate = if *nested {
                    entry.strip_prefix(path).unwrap_or(entry)
                } else {
                    Path::new(entry.file_name().unwrap_or_default())
                };
                !glob.is_match(candidate)
            }) {
                return true;
            }
            let Search::Grep { regex, .. } = search else {
                return push(entry, kind.is_dir(), None);
            };
            if !kind.is_file() {
                return true;
            }
            let bytes = match self.load_with_budget(entry, &mut remaining_bytes) {
                Ok(bytes) => bytes,
                Err(WorkToolError::SearchBudgetExhausted) => return false,
                Err(_) => {
                    skipped += 1;
                    return true;
                }
            };
            let Ok(text) = text(&bytes) else { return true };
            text.lines()
                .enumerate()
                .filter(|(_, line)| regex.is_match(line))
                .all(|(index, line)| {
                    let prefix = &line[..line.floor_char_boundary(2048)];
                    clipped_line |= prefix.len() < line.len();
                    push(entry, false, Some((index + 1, prefix)))
                })
        };
        let finished = if matches!(search, Search::Grep { .. })
            && let Ok(metadata) = self.dir.symlink_metadata(path)
            && metadata.is_file()
        {
            visit(path, metadata.file_type())
        } else {
            let dir = self.dir.open_dir(path)?;
            let prefix = if path == Path::new(".") {
                Path::new("")
            } else {
                path
            };
            let depth = if matches!(search, Search::List) {
                1
            } else {
                64
            };
            walk(&dir, prefix, depth, &mut visit)?
        };
        require_active(stop)?;
        let truncated = clipped_line || !finished;
        entries.sort_by(|left, right| {
            left["path"]
                .as_str()
                .cmp(&right["path"].as_str())
                .then_with(|| left["line"].as_u64().cmp(&right["line"].as_u64()))
        });
        Ok(json!({"entries": entries, "truncated": truncated, "skipped_files": skipped}))
    }
}

// Each recursive step is relative to an open directory capability. An
// ambient path walker would reopen paths that can change during a search.
fn walk(
    dir: &Dir,
    parent: &Path,
    depth: u8,
    visit: &mut impl FnMut(&Path, FileType) -> bool,
) -> io::Result<bool> {
    for entry in dir.entries()? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        let path = parent.join(&name);
        if !visit(&path, kind) {
            return Ok(false);
        }
        if depth > 1 && kind.is_dir() && name != ".git" {
            match dir.open_dir_nofollow(&name) {
                Ok(subdir) => {
                    if !walk(&subdir, &path, depth - 1, visit)? {
                        return Ok(false);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(true)
}
