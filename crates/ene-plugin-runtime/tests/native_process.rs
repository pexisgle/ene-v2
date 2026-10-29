//! Exercise the trusted Rust runner through the real OS isolation boundary.
//! Build ene-plugin-runner first, or point ENE_TEST_PLUGIN_RUNNER at that binary.
use ene_plugin::*;
use ene_plugin_runtime::{
    Package,
    worker::{DenyServices, Worker},
};
use serde_json::json;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(verify());
    runtime.shutdown_timeout(Duration::from_millis(100));
}
fn runner() -> PathBuf {
    let binary = std::env::var_os("ENE_TEST_PLUGIN_RUNNER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let name = format!("ene-plugin-runner{}", std::env::consts::EXE_SUFFIX);
            let current = std::env::current_exe().unwrap();
            // Support both Cargo's conventional deps directory and embedded-metadata layouts.
            current
                .ancestors()
                .skip(1)
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
                .expect("build ene-plugin-runner first or set ENE_TEST_PLUGIN_RUNNER")
        });
    assert!(
        binary.is_file(),
        "build ene-plugin-runner first or set ENE_TEST_PLUGIN_RUNNER: {}",
        binary.display()
    );
    binary
}
async fn verify() {
    let runner = runner();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    ene_platform::storage::create_private_dir_all(&source).unwrap();
    ene_platform::storage::write_atomic(
        &source.join("main.luau"),
        br#"
        return {initialize=function() return {} end,invoke=function(op)
            assert(os == nil or os.getenv == nil, "host environment must be inaccessible")
            assert(io == nil, "host files must be inaccessible")
            assert(package == nil, "native libraries must be inaccessible")
            if op.event.name == "wait" then while true do end end
            return {echo=op}
        end}
    "#,
    )
    .unwrap();
    ene_platform::storage::write_atomic(&source.join("manifest.json"), json!({
        "contract_version": CONTRACT_VERSION, "id": "runner-fixture", "name": "Runner fixture", "version": "1",
        "code_files": ["main.luau"], "entrypoints": [{"id": "behavior", "target": "host", "runtime": "luau",
        "file": "main.luau", "provides": ["character"], "uses": []}]
    }).to_string().as_bytes()).unwrap();
    let package = Package::load(&source).unwrap();
    let installed = package.install(&temp.path().join("cache")).unwrap();
    let cancel = CancellationToken::new();
    let mut worker = Worker::start(
        installed.clone(),
        "behavior".into(),
        &runner,
        Target::Host,
        7,
        &cancel,
    )
    .await
    .unwrap();
    let operation = Operation::Character {
        target: CharacterTarget {
            device_id: "device".into(),
            character_id: "character".into(),
        },
        event: CharacterEvent::Custom {
            name: "echo".into(),
            data: json!({"text": "日本語🙂"}),
        },
    };
    let (events, _received) = tokio::sync::mpsc::channel(4);
    let result = worker
        .invoke(
            "request".into(),
            operation.clone(),
            Arc::new(DenyServices),
            events,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(result["echo"], serde_json::to_value(operation).unwrap());
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
    });
    let (events, _received) = tokio::sync::mpsc::channel(4);
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        worker.invoke(
            "blocked".into(),
            Operation::Character {
                target: CharacterTarget {
                    device_id: "device".into(),
                    character_id: "character".into(),
                },
                event: CharacterEvent::Custom {
                    name: "wait".into(),
                    data: json!({}),
                },
            },
            Arc::new(DenyServices),
            events,
            &cancel,
        ),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(ene_plugin_runtime::Error::Cancelled)));
    drop(worker);

    // A mismatched contract terminates the real runner with an abnormal exit.
    let mut child =
        ene_plugin_runtime::worker::spawn(&installed, "behavior", &runner, Target::Host).unwrap();
    let (mut input, _output) = tokio::time::timeout(
        Duration::from_secs(10),
        ene_plugin_runtime::transport::host(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    input
        .send(&HostMessage::Initialize {
            contract_version: CONTRACT_VERSION + 1,
            call: Call {
                request_id: "invalid".into(),
                generation: 7,
            },
            entrypoint: "behavior".into(),
        })
        .await
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    println!("Rust runner/Luau isolation, Unicode IPC, cancellation and abnormal exit passed");
}
