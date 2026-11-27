//! The host reaps a never-initialized MCP child when its launch is invalidated.

use super::*;
use crate::host::{
    db::Store,
    mcp::supervisor::{Event, sync},
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

    for action in [
        "disable",
        "revision",
        "recreate",
        "shutdown",
        "failure",
        "world drop",
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(&root.path().join("host.sqlite3")).unwrap();
        let repo = store.settings_repo();
        let mut peer = PendingPeer::new(root.path(), "initial.pid");
        let mut row = Connection {
            id: ene_domain::ConnectionId::parse("pending-mcp").unwrap(),
            name: "pending".into(),
            enabled: true,
            kind: ConnectionKind::Mcp,
            config: ConnectionConfig::Mcp(peer.config()),
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
        };
        let settings = repo.update_settings(1, &[(row.clone(), 0)]).unwrap();
        let mut world = World::new();
        world.insert_resource(store.clone());
        world.insert_resource(HostRuntime(tokio::runtime::Handle::current()));
        world.init_resource::<HostState>();
        world.init_resource::<Mcp>();
        sync(&mut world);
        peer.started().await;
        let mcp = world.resource::<Mcp>().clone();
        let old_launch = opening_launch(&mcp, row.id.as_str());
        let mut replacement = PendingPeer::new(root.path(), "replacement.pid");
        match action {
            "disable" => {
                row.enabled = false;
                repo.update_settings(
                    settings.revision,
                    &[(row.clone(), old_launch.stamp.revision)],
                )
                .unwrap();
            }
            "revision" => {
                row.config = ConnectionConfig::Mcp(replacement.config());
                repo.update_settings(
                    settings.revision,
                    &[(row.clone(), old_launch.stamp.revision)],
                )
                .unwrap();
            }
            "recreate" => {
                let (settings, _) = repo
                    .delete_connection(&row.id, old_launch.stamp.revision)
                    .unwrap();
                row.config = ConnectionConfig::Mcp(replacement.config());
                repo.update_settings(settings.revision, &[(row.clone(), 0)])
                    .unwrap();
            }
            "shutdown" => world.resource::<HostState>().close(),
            "failure" => {
                assert!(world.resource::<HostState>().fail("test failure"));
            }
            "world drop" => {
                drop(mcp);
                drop(world);
                peer.stopped(action).await;
                continue;
            }
            _ => unreachable!(),
        }
        sync(&mut world);
        if matches!(action, "revision" | "recreate") {
            replacement.started().await;
        }
        peer.stopped(action).await;
        if matches!(action, "revision" | "recreate") {
            let new_launch = opening_launch(&mcp, row.id.as_str());
            assert_ne!(old_launch, new_launch);
            if action == "recreate" {
                assert!(old_launch.stamp.revision < new_launch.stamp.revision);
                assert_eq!(
                    old_launch.stamp.credential_generation,
                    new_launch.stamp.credential_generation
                );
                assert_ne!(
                    old_launch.stamp.created_revision,
                    new_launch.stamp.created_revision
                );
            }
            assert_late_failure_ignored(
                world.resource::<Mcp>(),
                row.id.as_str(),
                old_launch,
                &new_launch,
            );
            assert!(
                rustix::process::test_kill_process(replacement.process.as_ref().unwrap().0).is_ok()
            );
            world.resource::<HostState>().close();
            sync(&mut world);
            replacement.stopped("replacement shutdown").await;
        }
        assert!(mcp.lock().slots.is_empty());
    }
}

fn assert_late_failure_ignored(mcp: &Mcp, id: &str, old_launch: Launch, new_launch: &Launch) {
    mcp.lock().accept_event(
        &[(
            id.to_owned(),
            Desired {
                stamp: new_launch.stamp,
                ..want("pending")
            },
        )]
        .into(),
        Event::Failed {
            id: id.to_owned(),
            launch: old_launch,
            failure: ene_work::mcp::ConnectFailure::Transient("late failure".into()),
        },
        None,
        &tokio::runtime::Handle::current(),
    );
    assert_eq!(opening_launch(mcp, id), *new_launch);
}

fn opening_launch(mcp: &Mcp, id: &str) -> Launch {
    let supervisor = mcp.lock();
    let slot = &supervisor.slots[id];
    assert!(matches!(slot.phase, Phase::Opening { .. }));
    slot.launch.clone()
}
