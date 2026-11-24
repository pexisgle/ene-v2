//! Workspace operations executed by the desktop client for a work session.
//!
//! `workspace` owns the sandboxed file access every file tool shares and the
//! schema the host is offered; `shell` owns the one process the client spawns;
//! `helpers` holds the checks the tools share. The contract the host sees (the
//! capability names, the error type, the limits) stays here.

use std::{io, time::Duration};

mod helpers;
mod shell;
#[cfg(test)]
mod tests;
mod workspace;

pub(crate) use workspace::Workspace;

const FILE_LIMIT: usize = 4 * 1024 * 1024;
const IMAGE_LIMIT: usize = 512 * 1024;
const OUTPUT_LIMIT: usize = 24 * 1024;
const SCAN_LIMIT: usize = 20_000;
const GREP_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESULTS: usize = 500;
const COMMAND_TIMEOUT: u64 = 30;
/// How long `Drop` waits for a killed tool process to be collected.
///
/// `start_kill` is asynchronous, so the status is not available immediately; a
/// short poll is enough for a process that is already being torn down.
const REAP_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkToolError {
    #[error("the workspace has changed")]
    WorkspaceChanged,
    #[error("unknown workspace tool")]
    UnknownTool,
    #[error("file writer is unavailable")]
    WriterUnavailable,
    #[error("path must be a string")]
    PathNotString,
    #[error("path is required")]
    MissingPath,
    #[error("path must stay relative to the workspace")]
    InvalidPath,
    #[error("{0} must be a string")]
    NotString(&'static str),
    #[error("{0} must be a positive integer")]
    NotPositiveInt(&'static str),
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
    #[error("the requested shell is unavailable")]
    ShellUnavailable,
    #[error("command is empty")]
    EmptyCommand,
    #[error("the shell process is unavailable")]
    ShellProcessUnavailable,
    #[error("the shell process pipes are unavailable")]
    ShellPipesUnavailable,
    #[error("command exceeded {COMMAND_TIMEOUT} seconds")]
    CommandTimedOut,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Regex(#[from] regex::Error),
    #[error(transparent)]
    Glob(#[from] globset::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

pub(crate) const PREFIX: &str = "work.";
pub const WORKSPACE_FILE_CAPABILITIES: &[&str] = &[
    "work.read",
    "work.write",
    "work.edit",
    "work.grep",
    "work.find",
    "work.ls",
];

#[cfg(windows)]
const SHELL: &str = "powershell";
#[cfg(not(windows))]
const SHELL: &str = "bash";

#[cfg(windows)]
pub(crate) const WORKSPACE_SHELL_CAPABILITY: &str = "work.powershell";
#[cfg(not(windows))]
pub(crate) const WORKSPACE_SHELL_CAPABILITY: &str = "work.bash";
