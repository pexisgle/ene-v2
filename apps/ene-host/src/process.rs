use anyhow::{Context, Result};
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use std::process::Stdio;
use tokio::process::{ChildStdin, ChildStdout};

pub(crate) struct Child(Option<Box<dyn ChildWrapper>>);

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _already_exited = child.start_kill();
            tokio::spawn(async move {
                let _reaped = child.wait().await;
            });
        }
    }
}

pub(crate) fn spawn(executable: &str, args: &[String]) -> Result<(Child, ChildStdin, ChildStdout)> {
    let mut command = rmcp::transport::which_command(executable)?;
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = launch(command)?;
    let inner = child.0.as_mut().context("child missing")?.inner_mut();
    let input = inner.stdin().take().context("child stdin missing")?;
    let output = inner.stdout().take().context("child stdout missing")?;
    Ok((child, input, output))
}

fn launch(mut command: tokio::process::Command) -> Result<Child> {
    command.env_clear();
    for name in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "SystemRoot",
        "TEMP",
        "TMPDIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.wrap(process_wrap::tokio::KillOnDrop);
    Ok(Child(Some(command.spawn()?)))
}

pub(crate) async fn run_task(
    executable: &str,
    args: &[String],
    root: &std::path::Path,
) -> Result<String> {
    use tokio::io::AsyncReadExt as _;
    let mut command = rmcp::transport::which_command(executable)?;
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = launch(command)?;
    let inner = child.0.as_mut().context("child missing")?;
    let stdout = inner
        .inner_mut()
        .stdout()
        .take()
        .context("child stdout missing")?;
    let stderr = inner
        .inner_mut()
        .stderr()
        .take()
        .context("child stderr missing")?;
    async fn output(reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut reader = reader.take(16_385);
        reader.read_to_end(&mut bytes).await?;
        if bytes.len() > 16_384 {
            return Err(std::io::Error::other("task output exceeds limit"));
        }
        Ok(bytes)
    }
    let (status, stdout, stderr) = tokio::try_join!(inner.wait(), output(stdout), output(stderr))?;
    Ok(serde_json::json!({
        "exit_code": status.code(),
        "stdout": crate::model::truncate(&String::from_utf8_lossy(&stdout), 5000),
        "stderr": crate::model::truncate(&String::from_utf8_lossy(&stderr), 5000),
    })
    .to_string())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn retiring_a_connector_also_kills_its_child_processes() -> Result<()> {
        let script = vec!["-c".into(), "sleep 60 & printf '%s\\n' \"$!\"; wait".into()];
        let (process, _input, output) = spawn("sh", &script)?;
        let mut lines = BufReader::new(output).lines();
        let pid = tokio::time::timeout(std::time::Duration::from_secs(3), lines.next_line())
            .await??
            .context("child pid missing")?
            .parse::<u32>()?;
        drop(process);
        let process = format!("/proc/{pid}/stat");
        tokio::time::timeout(std::time::Duration::from_secs(3), async move {
            loop {
                let running = std::fs::read_to_string(&process).is_ok_and(|status| {
                    status
                        .rsplit_once(") ")
                        .is_some_and(|(_, rest)| !rest.starts_with('Z'))
                });
                if !running {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        })
        .await?;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{io::Write as _, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};

    const FIXTURE: &str = "process::windows_tests::job_fixture";
    const PID_MARKER: &str = "ene-job-child=";

    #[tokio::test]
    async fn retiring_a_connector_terminates_its_job_object() -> Result<()> {
        let executable = std::env::current_exe()?;
        let executable = executable.to_str().context("test executable path")?;
        let args = ["--ignored", "--nocapture", "--exact", FIXTURE].map(str::to_owned);
        let (process, input, output) = spawn(executable, &args)?;
        drop(input);
        let mut lines = BufReader::new(output).lines();
        let pid = tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(line) = lines.next_line().await? {
                if let Some((_, value)) = line.split_once(PID_MARKER) {
                    return Ok::<_, anyhow::Error>(value.trim().parse::<u32>()?);
                }
            }
            anyhow::bail!("fixture exited without reporting its child")
        })
        .await
        .context("fixture did not report its child")??;
        assert!(running(pid).await?);
        drop(process);
        tokio::time::timeout(Duration::from_secs(15), async move {
            while running(pid).await? {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("fixture child remained alive after the job closed")??;
        Ok(())
    }

    #[test]
    #[ignore = "launched by the Windows Job Object test"]
    fn job_fixture() -> Result<()> {
        if !std::env::args().any(|arg| arg == FIXTURE) {
            return Ok(());
        }
        let mut child = std::process::Command::new("ping.exe")
            .args(["-n", "60", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        writeln!(std::io::stdout().lock(), "{PID_MARKER}{}", child.id())?;
        child.wait()?;
        Ok(())
    }

    async fn running(pid: u32) -> Result<bool> {
        let filter = format!("PID eq {pid}");
        let output = tokio::process::Command::new("tasklist.exe")
            .args(["/fi", &filter, "/fo", "csv", "/nh"])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(output.status.success(), "tasklist failed");
        let field = format!("\"{pid}\"");
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split(',').nth(1) == Some(field.as_str())))
    }
}
