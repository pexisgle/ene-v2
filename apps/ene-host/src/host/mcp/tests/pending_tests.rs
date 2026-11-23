//! The host reaps a never-initialized MCP child when its launch is invalidated.

use super::*;
use crate::host::{
    db::Store,
    mcp::supervisor::{Event, accept_event, sync},
    resources::HostRuntime,
    settings::{Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint},
};

pub(super) struct PendingPeer {
    marker: std::path::PathBuf,
    process: Option<(rustix::process::Pid, rustix::fd::OwnedFd)>,
}

impl PendingPeer {
    pub(super) fn new(root: &std::path::Path, name: &str) -> Self {
        Self {
            marker: root.join(name),
            process: None,
        }
    }

    pub(super) fn config(&self) -> crate::host::settings::McpConnection {
        crate::host::settings::McpConnection {
            endpoint: Endpoint {
                adapter: "mcp".into(),
                options: "{}".into(),
                ..Default::default()
            },
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s' \"$$\" > \"$1\"; exec sleep 300".into(),
                "owned-mcp-peer".into(),
                self.marker.to_str().unwrap().into(),
            ],
            ..Default::default()
        }
    }

    pub(super) async fn started(&mut self) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&self.marker)
                    .ok()
                    .and_then(|text| text.parse::<i32>().ok())
                    .and_then(rustix::process::Pid::from_raw)
                {
                    let fd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                        .unwrap();
                    self.process = Some((pid, fd));
                    assert!(rustix::process::test_kill_process(pid).is_ok());
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the owned never-initializing peer records its PID");
    }

    pub(super) async fn stopped(&mut self, action: &str) {
        let pid = self.process.as_ref().unwrap().0;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("owned pending MCP PID {pid} survived {action}"));
        self.process = None;
        eprintln!("owned pending MCP PID {pid} disappeared after {action}");
    }
}

impl Drop for PendingPeer {
    fn drop(&mut self) {
        if let Some((_, fd)) = &self.process {
            let _discarded = rustix::process::pidfd_send_signal(fd, rustix::process::Signal::KILL);
        }
    }
}

#[tokio::test]
async fn pending_launch_invalidation_reaps_never_initialized_children() {
    use crate::host::resources::HostState;

    for action in ["disable", "revision", "shutdown", "failure", "world drop"] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let mut peer = PendingPeer::new(root.path(), "initial.pid");
        let mut row = Connection {
            id: "pending-mcp".into(),
            name: "pending".into(),
            enabled: true,
            kind: ConnectionKind::Mcp,
            config: ConnectionConfig::Mcp(peer.config()),
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
        };
        store
            .settings_repo()
            .update_settings(1, &[(row.clone(), 0)])
            .unwrap();
        let mut world = World::new();
        world.insert_resource(store.clone());
        world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        world.init_resource::<HostState>();
        world.init_resource::<McpSupervisor>();
        sync(&mut world);
        peer.started().await;
        assert!(world.resource::<McpSupervisor>().servers.0.is_empty());
        let old_launch = world.resource::<McpSupervisor>().connecting.0[&row.id]
            .launch
            .clone();
        let mut replacement = PendingPeer::new(root.path(), "replacement.pid");
        match action {
            "disable" => {
                row.enabled = false;
                store
                    .settings_repo()
                    .update_settings(2, &[(row.clone(), 1)])
                    .unwrap();
            }
            "revision" => {
                row.config = ConnectionConfig::Mcp(replacement.config());
                store
                    .settings_repo()
                    .update_settings(2, &[(row.clone(), 1)])
                    .unwrap();
            }
            "shutdown" => world.resource_mut::<HostState>().closing = true,
            "failure" => world.resource_mut::<HostState>().failed = Some("test failure".into()),
            "world drop" => {
                drop(world);
                peer.stopped(action).await;
                continue;
            }
            _ => unreachable!(),
        }
        sync(&mut world);
        if action == "revision" {
            replacement.started().await;
        }
        peer.stopped(action).await;
        assert!(world.resource::<McpSupervisor>().servers.0.is_empty());
        if action == "revision" {
            let new_launch = world.resource::<McpSupervisor>().connecting.0[&row.id]
                .launch
                .clone();
            assert_ne!(old_launch, new_launch);
            let mut supervisor = std::mem::take(&mut *world.resource_mut::<McpSupervisor>());
            accept_event(
                &mut world,
                &mut supervisor,
                &[(
                    row.id.clone(),
                    Desired {
                        revision: 2,
                        ..want("pending")
                    },
                )],
                Event::Failed {
                    id: row.id.clone(),
                    launch: old_launch,
                    reason: "late failure".into(),
                    config_error: false,
                },
                &tokio::runtime::Handle::current(),
            );
            *world.resource_mut::<McpSupervisor>() = supervisor;
            assert_eq!(
                world.resource::<McpSupervisor>().connecting.0[&row.id].launch,
                new_launch
            );
            assert!(
                rustix::process::test_kill_process(replacement.process.as_ref().unwrap().0).is_ok()
            );
            world.resource_mut::<HostState>().closing = true;
            sync(&mut world);
            replacement.stopped("replacement shutdown").await;
        }
        assert!(world.resource::<McpSupervisor>().connecting.0.is_empty());
    }
}
