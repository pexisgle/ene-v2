//! Bounded search through open workspace directory capabilities.

use super::{
    GREP_BYTES, MAX_RESULTS, OUTPUT_LIMIT, SCAN_LIMIT, WorkToolError,
    helpers::{relative, text},
    workspace::Workspace,
};
use cap_fs_ext::DirExt as _;
use cap_std::fs::{Dir, FileType};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{io, num::NonZeroUsize, path::Path};

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
    ) -> Result<Value, WorkToolError> {
        let path = relative(&browse.path)?;
        let limit = browse.limit.get().min(MAX_RESULTS);
        let mut entries = Vec::new();
        let mut remaining_bytes = GREP_BYTES;
        let mut output_bytes = 0;
        let mut scanned = 0;
        let mut truncated = false;
        let mut skipped = 0;
        let glob = match search {
            Search::List => None,
            Search::Find(glob) => Some(glob),
            Search::Grep { glob, .. } => glob,
        };
        let start = path;
        let mut visit = |path: &Path, kind: FileType| -> bool {
            scanned += 1;
            if scanned > SCAN_LIMIT {
                truncated = true;
                return false;
            }
            if glob.is_some_and(|(glob, nested)| {
                let candidate = if *nested {
                    path.strip_prefix(start).unwrap_or(path)
                } else {
                    Path::new(path.file_name().unwrap_or_default())
                };
                !glob.is_match(candidate)
            }) {
                return true;
            }
            if let Search::Grep { regex, .. } = search {
                if !kind.is_file() {
                    return true;
                }
                let Ok(bytes) = self.load(path) else {
                    skipped += 1;
                    return true;
                };
                if bytes.len() > remaining_bytes {
                    truncated = true;
                    return false;
                }
                remaining_bytes -= bytes.len();
                let Ok(text) = text(&bytes) else { return true };
                for (index, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        let prefix = &line[..line.floor_char_boundary(2048)];
                        truncated |= prefix.len() < line.len();
                        let line = prefix;
                        output_bytes += path.as_os_str().len() + line.len();
                        if entries.len() == limit || output_bytes > OUTPUT_LIMIT {
                            truncated = true;
                            return false;
                        }
                        entries.push(json!({"path": path.to_string_lossy(), "line": index + 1, "text": line}));
                    }
                }
            } else {
                output_bytes += path.as_os_str().len();
                if entries.len() == limit || output_bytes > OUTPUT_LIMIT {
                    truncated = true;
                    return false;
                }
                entries.push(json!({"path": path.to_string_lossy(), "directory": kind.is_dir()}));
            }
            true
        };
        if matches!(search, Search::Grep { .. })
            && let Ok(metadata) = self.dir.symlink_metadata(path)
            && metadata.is_file()
        {
            visit(path, metadata.file_type());
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
            let _finished = walk(&dir, prefix, depth, &mut visit)?;
        }
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
