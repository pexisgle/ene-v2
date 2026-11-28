use std::io;

mod files;
mod helpers;
mod search;

pub(crate) use files::{PREFIX, Workspace};

const FILE_LIMIT: usize = 4 * 1024 * 1024;
const IMAGE_LIMIT: usize = 512 * 1024;
const OUTPUT_LIMIT: usize = 24 * 1024;
const SCAN_LIMIT: usize = 20_000;
const GREP_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkToolError {
    #[error("the workspace has changed")]
    WorkspaceChanged,
    #[error("unknown workspace tool")]
    UnknownTool,
    #[error("path must stay relative to the workspace")]
    InvalidPath,
    #[error("invalid workspace tool arguments: {0}")]
    Arguments(#[from] serde_json::Error),
    #[error("image exceeds the 512 KiB limit")]
    ImageTooLarge,
    #[error("line exceeds the output limit. Use grep or a shell command to inspect part of it.")]
    LineTooLong,
    #[error("file is binary")]
    BinaryFile,
    #[error("file is not UTF-8")]
    NotUtf8,
    #[error("edits must be a nonempty array")]
    EmptyEdits,
    #[error("oldText must not be empty")]
    EmptyOldText,
    #[error("oldText did not match the file")]
    OldTextNotFound,
    #[error("oldText matched more than once")]
    OldTextNotUnique,
    #[error("file exceeds the 4 MiB limit")]
    FileTooLarge,
    #[error("file contents changed since the last read")]
    StaleFile,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    Glob(#[from] globset::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}
