//! One host-process harness for the integration tests.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// The host binary with an isolated state, plugin and runtime directory.
pub fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ene-host"));
    command
        .env("XDG_RUNTIME_DIR", root)
        .env("ENE_HOST_STATE_DIR", root.join("state"))
        .env("ENE_PLUGIN_DIR", root.join("plugins"))
        .env("ENE_HOST_SESSION_CREDENTIALS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

/// Starts the host and resolves its published connection descriptor.
pub async fn start(root: &Path) -> (Child, serde_json::Value) {
    let mut child = command(root).spawn().unwrap();
    let descriptor = root.join("ene-host/connection.json");
    let connection = timeout(Duration::from_secs(20), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "host exited during startup"
            );
            match std::fs::read(&descriptor) {
                Ok(bytes) => break serde_json::from_slice(&bytes).unwrap(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("connection descriptor: {error}"),
            }
        }
    })
    .await
    .expect("host startup deadline");
    assert_eq!(
        std::fs::metadata(&descriptor).unwrap().permissions().mode() & 0o777,
        0o600
    );
    (child, connection)
}

/// Stops the host and asserts that its descriptor is gone.
pub async fn stop(child: &mut Child, root: &Path) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(
        timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(!root.join("ene-host/connection.json").exists());
}
