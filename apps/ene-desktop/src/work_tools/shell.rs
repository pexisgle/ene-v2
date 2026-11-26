use super::{COMMAND_TIMEOUT, OUTPUT_LIMIT, REAP_TIMEOUT, WorkToolError, workspace::Workspace};
#[cfg(windows)]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use serde_json::{Value, json};
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt as _};

impl Workspace {
    pub(super) async fn shell(&self, command: &str) -> Result<Value, WorkToolError> {
        let program = self.shell.as_ref().ok_or(WorkToolError::ShellUnavailable)?;
        if command.trim().is_empty() {
            return Err(WorkToolError::EmptyCommand);
        }
        let mut process = CommandWrap::with_new(program, |child| {
            child
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(not(windows))]
            child.args(["--noprofile", "--norc", "-c", command]);
            #[cfg(windows)]
            {
                child.creation_flags(0x08000000); // CREATE_NO_WINDOW
                let script =
                    format!("[Console]::OutputEncoding = [Text.UTF8Encoding]::new(); {command}");
                let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
                child
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-EncodedCommand",
                    ])
                    .arg(STANDARD.encode(bytes));
            }
        });
        #[cfg(unix)]
        process.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        process.wrap(process_wrap::tokio::JobObject);
        process.wrap(KillOnDrop);
        let mut child = scopeguard::guard(process.spawn()?, |mut child| {
            reap_killed_child(child.as_mut());
        });
        let (Some(stdout), Some(stderr)) = (child.stdout().take(), child.stderr().take()) else {
            return Err(WorkToolError::ShellPipesUnavailable);
        };
        let outcome = tokio::time::timeout(
            Duration::from_secs(COMMAND_TIMEOUT),
            Box::pin(async { tokio::try_join!(child.wait(), capture(stdout), capture(stderr)) }),
        )
        .await;
        let (status, (stdout, stdout_cut), (stderr, stderr_cut)) = match outcome {
            Ok(result) => result?,
            Err(_) => return Err(WorkToolError::CommandTimedOut),
        };
        drop(scopeguard::ScopeGuard::into_inner(child));
        Ok(json!({
            "success": status.success(), "exit_code": status.code(), "stdout": stdout, "stderr": stderr,
            "truncated": stdout_cut || stderr_cut
        }))
    }
}

fn reap_killed_child(child: &mut dyn ChildWrapper) {
    // Kill the group or job when the device stream drops the future, then
    // poll for its exit status within the synchronous drop budget.
    ene_domain::warn_if_err(child.start_kill(), "tool process termination");
    let deadline = std::time::Instant::now() + REAP_TIMEOUT;
    while std::time::Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        std::thread::sleep(Duration::from_millis(5));
    }
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> io::Result<(String, bool)> {
    let mut output = Vec::new();
    (&mut stream)
        .take(OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut output)
        .await?;
    let truncated = output.len() > OUTPUT_LIMIT;
    if truncated {
        output.truncate(OUTPUT_LIMIT);
        tokio::io::copy(&mut stream, &mut tokio::io::sink()).await?;
    }
    Ok((String::from_utf8_lossy(&output).into_owned(), truncated))
}
