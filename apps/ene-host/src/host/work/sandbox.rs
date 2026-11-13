//! Process isolation for the external ACP child process.
//!
//! The child is the only process this host starts that is not under the ECS
//! tool layer, so it is the only one that needs a kernel-level boundary. On
//! Linux the boundary is `bwrap --unshare-net`: the child runs in its own
//! network namespace and cannot open a direct outbound connection. Alpha has no
//! host-side proxy bridge yet (docs/design/permissions.md §2.1), so the agent
//! can only reach a local model; the plan says so in a user-visible note rather
//! than claiming an isolation the build does not provide.
//!
//! `cap-std` is deliberately unused here: it protects the host's own file I/O
//! and does not sandbox a spawned process (permissions.md §2.1 note).
use std::path::{Path, PathBuf};

/// A connection option value that disables isolation for this ACP connection.
pub(crate) const SANDBOX_OPTION: &str = "sandbox";
/// The notice code shown when the child runs without a sandbox.
pub(crate) const NON_ISOLATED_CODE: &str = "acp_sandbox_unavailable";
/// The notice code shown when bwrap isolates the child's network.
pub(crate) const ISOLATED_CODE: &str = "acp_sandbox_enabled";

/// How the ACP child must be started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SandboxPlan {
    /// The program to spawn (the sandbox wrapper, when isolated).
    pub(crate) program: PathBuf,
    /// The arguments for `program`; they end with the agent command and args.
    pub(crate) args: Vec<String>,
    /// Whether a kernel isolation mechanism wraps the child.
    pub(crate) isolated: bool,
    /// A user-visible explanation of the boundary actually applied.
    pub(crate) notice: String,
    /// The notice code that goes with `notice`.
    pub(crate) notice_code: &'static str,
}

impl SandboxPlan {
    pub(crate) fn was_isolated(&self) -> bool {
        self.isolated
    }
}

/// Builds the start plan for `command` from the connection options.
///
/// `options` is the JSON object text of the ACP connection endpoint. The only
/// key read is `sandbox`: `"none"` disables the wrapper explicitly, `"bwrap"`
/// demands it, and anything else or a missing key selects it when available.
pub(crate) fn plan(command: &str, args: &[String], cwd: &Path, options: &str) -> SandboxPlan {
    let requested = requested_mode(options);
    if requested.as_deref() == Some("none") {
        return non_isolated(
            command,
            args,
            "ACP接続の `sandbox` オプションにより、子プロセスは非隔離で起動します。",
        );
    }
    let requested_bwrap = requested.as_deref() == Some("bwrap");
    #[cfg(target_os = "linux")]
    if let Some(plan) = bwrap_plan(command, args, cwd, requested_bwrap) {
        return plan;
    }
    let _ = requested_bwrap;
    let reason = if requested_bwrap {
        "`sandbox: bwrap` が指定されましたが bwrap が見つからないため、ACP子プロセスは非隔離で起動します。"
    } else {
        "この環境では bwrap を利用できないため、ACP子プロセスは非隔離で起動します。"
    };
    non_isolated(command, args, reason)
}

/// Reads the optional `sandbox` switch from a connection options JSON object.
pub(crate) fn requested_mode(options: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(options).ok()?;
    let mode = value
        .get(SANDBOX_OPTION)?
        .as_str()?
        .trim()
        .to_ascii_lowercase();
    (!mode.is_empty()).then_some(mode)
}

fn non_isolated(command: &str, args: &[String], reason: &str) -> SandboxPlan {
    SandboxPlan {
        program: PathBuf::from(command),
        args: args.to_vec(),
        isolated: false,
        notice: reason.to_owned(),
        notice_code: NON_ISOLATED_CODE,
    }
}

#[cfg(target_os = "linux")]
fn bwrap_plan(
    command: &str,
    args: &[String],
    cwd: &Path,
    _requested_bwrap: bool,
) -> Option<SandboxPlan> {
    let bwrap = which::which("bwrap").ok()?;
    let cwd = cwd.to_str()?;
    let mut wrapped = vec![
        "--die-with-parent".to_owned(),
        "--unshare-net".to_owned(),
        "--unshare-pid".to_owned(),
        "--bind".to_owned(),
        "/".to_owned(),
        "/".to_owned(),
        "--proc".to_owned(),
        "/proc".to_owned(),
        "--dev-bind".to_owned(),
        "/dev".to_owned(),
        "/dev".to_owned(),
        "--chdir".to_owned(),
        cwd.to_owned(),
        "--".to_owned(),
        command.to_owned(),
    ];
    wrapped.extend(args.iter().cloned());
    Some(SandboxPlan {
        program: bwrap,
        args: wrapped,
        isolated: true,
        notice: "ACP子プロセスは bwrap --unshare-net で起動します。ネットワークは分離されており、\
                 外部APIへはホスト側プロキシ経由でのみ到達できる設計です（Alphaではプロキシ未実装のため、\
                 外部モデルを使うエージェントは接続できない場合があります）。"
            .to_owned(),
        notice_code: ISOLATED_CODE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sandbox_option_parses_to_a_mode() {
        assert_eq!(requested_mode(r#"{"sandbox":"none"}"#), Some("none".into()));
        assert_eq!(
            requested_mode(r#"{"sandbox":" BWRAP "}"#),
            Some("bwrap".into())
        );
        assert_eq!(requested_mode("{}"), None);
        assert_eq!(requested_mode("not json"), None);
    }

    #[test]
    fn an_explicitly_disabled_sandbox_never_claims_isolation() {
        let plan = plan(
            "opencode",
            &["--stdio".to_owned()],
            Path::new("/tmp"),
            r#"{"sandbox":"none"}"#,
        );
        assert!(!plan.was_isolated());
        assert_eq!(plan.program, PathBuf::from("opencode"));
        assert_eq!(plan.args, vec!["--stdio".to_owned()]);
        assert_eq!(plan.notice_code, NON_ISOLATED_CODE);
        assert!(plan.notice.contains("非隔離"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_bwrap_plan_wraps_the_command_and_unshares_the_network() {
        if which::which("bwrap").is_err() {
            return;
        }
        let plan = plan("opencode", &["--stdio".to_owned()], Path::new("/tmp"), "{}");
        assert!(plan.was_isolated());
        assert_eq!(plan.notice_code, ISOLATED_CODE);
        assert!(plan.args.iter().any(|arg| arg == "--unshare-net"));
        assert!(plan.args.iter().any(|arg| arg == "opencode"));
    }
}
