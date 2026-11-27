//! Stdio transport and ownership of the server's process group.
use super::until_cancelled;
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::{TokioChildProcess, Transport},
};
use std::io;
use tokio_util::sync::CancellationToken;

pub(super) struct CancellableStdio {
    child: Option<TokioChildProcess>,
    stop: CancellationToken,
}

impl CancellableStdio {
    pub(super) fn new(child: TokioChildProcess, stop: CancellationToken) -> Self {
        Self {
            child: Some(child),
            stop,
        }
    }
}

impl Transport<RoleClient> for CancellableStdio {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let stop = self.stop.clone();
        let send = self.child.as_mut().map(|child| child.send(item));
        async move {
            let send = send.ok_or(io::ErrorKind::NotConnected)?;
            until_cancelled(&stop, send)
                .await
                .ok_or(io::ErrorKind::Interrupted)?
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        until_cancelled(&self.stop, self.child.as_mut()?.receive())
            .await
            .flatten()
    }

    async fn close(&mut self) -> io::Result<()> {
        match self.child.take() {
            Some(child) => close(child).await,
            None => Ok(()),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "freebsd")))]
async fn close(mut child: TokioChildProcess) -> io::Result<()> {
    child.graceful_shutdown().await
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
use unix::close;

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
mod unix {
    use super::{TokioChildProcess, io};
    use process_wrap::tokio::ChildWrapper;
    use rustix::{
        io::{Errno, retry_on_intr},
        process::{Pid, WaitId, WaitIdOptions, WaitIdStatus, waitid},
    };
    use std::time::Duration;
    use tokio::signal::unix::{SignalKind, signal};

    const CLOSE_BUDGET: Duration = Duration::from_secs(3);
    const OBSERVE_EXIT: WaitIdOptions = WaitIdOptions::EXITED
        .union(WaitIdOptions::NOWAIT)
        .union(WaitIdOptions::NOHANG);

    // Drop must still signal the owned group if graceful close gets cancelled.
    struct Group(Option<Box<dyn ChildWrapper>>);

    impl Drop for Group {
        fn drop(&mut self) {
            if let Some(child) = &mut self.0 {
                ene_domain::debug_if_err(kill_group(child.as_mut()), "MCP stdio cleanup");
            }
        }
    }

    pub(super) async fn close(transport: TokioChildProcess) -> io::Result<()> {
        // Consuming the transport closes its pipes before we wait for EOF.
        let mut group = Group(transport.into_inner());
        if let Some(child) = &group.0
            && let Ok(mut exits) = signal(SignalKind::child())
        {
            let pid = leader(child.as_ref())?;
            let _grace = tokio::time::timeout(CLOSE_BUDGET, async {
                while leader_status(pid).is_ok_and(|status| status.is_none()) {
                    if exits.recv().await.is_none() {
                        break;
                    }
                }
            })
            .await;
        }
        if let Some(mut child) = group.0.take() {
            kill_group(child.as_mut())?;
            tokio::time::timeout(CLOSE_BUDGET, child.wait()).await??;
        }
        Ok(())
    }

    fn leader(child: &dyn ChildWrapper) -> io::Result<Pid> {
        child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other("MCP process group no longer has an owned leader"))
    }

    fn kill_group(child: &mut dyn ChildWrapper) -> io::Result<()> {
        // Keep the leader unreaped until after the signal. If another reaper
        // took it, do not signal a process group number we no longer own.
        leader_status(leader(child)?)?;
        match child.start_kill() {
            Err(error) if error.raw_os_error() == Some(Errno::SRCH.raw_os_error()) => Ok(()),
            outcome => outcome,
        }
    }

    fn leader_status(pid: Pid) -> rustix::io::Result<Option<WaitIdStatus>> {
        retry_on_intr(|| waitid(WaitId::Pid(pid), OBSERVE_EXIT))
    }
}
