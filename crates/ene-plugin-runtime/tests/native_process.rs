//! A native executable fixture with real stdio and OS isolation on both targets.
use ene_ipc::plugin::*;
use ene_plugin_runtime::{
    Package,
    worker::{DenyServices, Worker},
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match std::env::args().nth(1).as_deref() {
            Some("--worker") => fixture().await,
            Some("--legacy") => provider_fixture().await,
            Some("--exit-259") => std::process::exit(259),
            _ => verify().await,
        }
    });
    runtime.shutdown_timeout(Duration::from_millis(100));
}
async fn fixture() {
    let (mut reader, mut writer) =
        ene_plugin_runtime::transport::worker(tokio::io::stdin(), tokio::io::stdout())
            .await
            .unwrap();
    while let Some(message) = reader.receive().await.unwrap() {
        let output = match message {
            HostMessage::Initialize {
                call,
                protocol_version,
                ..
            } => WorkerMessage::Ready {
                call,
                protocol_version,
                capabilities: json!({}),
            },
            HostMessage::Invoke { call, operation } => {
                if matches!(&operation, Operation::Character { event: CharacterEvent::Custom { name, .. } } if name == "wait")
                {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
                WorkerMessage::Complete {
                    call,
                    result: json!({"echo": operation}),
                }
            }
            HostMessage::Shutdown | HostMessage::Cancel { .. } => return,
            _ => panic!("unexpected worker request"),
        };
        writer.send(&output).await.unwrap();
    }
}
struct FixtureProvider;
use ene_ipc::{
    provider::{self, Provider, ProviderError},
    remoc::{self, ConnectExt, rtc::ServerShared},
};
impl provider::Provider for FixtureProvider {
    async fn capabilities(&self) -> Result<provider::Capabilities, ProviderError> {
        Ok(provider::Capabilities {
            streaming_text: true,
            ..Default::default()
        })
    }
    async fn list_models(
        &self,
        _: String,
        _: serde_json::Value,
        _: provider::BrokerClient,
    ) -> Result<Vec<provider::Model>, ProviderError> {
        Err(ProviderError::InvalidInput)
    }
    async fn generate(
        &self,
        _: provider::GenerationRequest,
        _: provider::BrokerClient,
        _: provider::Sender<String>,
    ) -> Result<String, ProviderError> {
        Err(ProviderError::InvalidInput)
    }
}
async fn provider_fixture() {
    let (server, client) =
        provider::ProviderServerShared::<_, provider::Codec>::new(Arc::new(FixtureProvider));
    remoc::Connect::io::<_, _, provider::Client, provider::Client, remoc::codec::Default>(
        remoc::Cfg::default(),
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .provide(client)
    .await
    .unwrap();
    server.serve().await.unwrap();
}

async fn verify() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    ene_platform::storage::create_private_dir_all(&source).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), source.join("probe.exe")).unwrap();
    std::fs::write(source.join("manifest.json"), json!({
        "package_version": 1, "api_version": 1, "id": "native-fixture", "name": "Native fixture", "version": "1",
        "code_files": ["probe.exe"], "entrypoints": [{"id": "behavior", "target": "host", "runtime": "process",
        "file": "probe.exe", "command": ["/plugin/probe.exe", "--worker"], "provides": ["character"], "uses": []}]
    }).to_string()).unwrap();
    let package = Package::load(&source).unwrap();
    let cancel = CancellationToken::new();
    let mut worker = Worker::start(
        package.install(&temp.path().join("cache")).unwrap(),
        "behavior".into(),
        std::path::Path::new("unused"),
        Target::Host,
        7,
        &cancel,
    )
    .await
    .unwrap();
    let operation = Operation::Character {
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

    let mut child = ene_platform::sandbox::spawn(
        &source,
        &["/plugin/probe.exe".into(), "--legacy".into()],
        None,
    )
    .unwrap();
    let client: provider::Client =
        remoc::Connect::io::<_, _, provider::Client, provider::Client, remoc::codec::Default>(
            remoc::Cfg::default(),
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
        )
        .consume()
        .await
        .unwrap();
    assert!(client.capabilities().await.unwrap().streaming_text);
    drop(client);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(child);
    let mut child = ene_platform::sandbox::spawn(
        &source,
        &["/plugin/probe.exe".into(), "--exit-259".into()],
        None,
    )
    .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    #[cfg(windows)]
    assert_eq!(status.code(), Some(259));
    #[cfg(unix)]
    assert_eq!(status.code(), Some(3));
    println!(
        "native Process and Remoc provider isolation, Unicode IPC, cancellation and abnormal exit passed"
    );
}
