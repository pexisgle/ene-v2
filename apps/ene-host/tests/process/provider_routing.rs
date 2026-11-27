use super::{discovery, register_credential, spawn, stop};
use axum::{Router, extract::State, http::HeaderMap, routing::post};
use connectrpc::client::HttpClient;
use ene_rpc as w;
use std::{os::unix::fs::PermissionsExt as _, path::Path, time::Duration};
use tokio::sync::mpsc;

async fn answer(
    State(seen): State<mpsc::UnboundedSender<(bool, bool)>>,
    headers: HeaderMap,
) -> ([(&'static str, &'static str); 1], &'static str) {
    seen.send((
        headers
            .get("x-fixture-key")
            .is_some_and(|key| key == "dummy-key"),
        headers.contains_key("authorization"),
    ))
    .unwrap();
    (
        [("content-type", "text/event-stream")],
        concat!(
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"model\":\"test\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Fixture reply\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"model\":\"test\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ),
    )
}

fn fixture(source: &Path, origin: &str) {
    let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ene_openrouter_wasm.wasm");
    std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(wasm, source.join("main.wasm")).unwrap();
    std::fs::write(
        source.join("manifest.json"),
        serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "openrouter", "name": "Fixture", "version": "1",
            "entrypoints": [{"id": "main", "target": "host", "file": "main.wasm",
                "provides": ["llm"], "uses": ["http"], "provider": {
                    "id": "fixture-provider", "endpoint": format!("{origin}/v1"),
                    "auth": {"type": "header", "name": "x-fixture-key"}, "destinations": [origin]
                }}]
        })
        .to_string(),
    )
    .unwrap();
}

async fn configure(
    settings: &w::SettingsServiceClient<HttpClient>,
    source: &Path,
    origin: &str,
) -> w::ModelAssignment {
    settings
        .install_plugin(w::InstallPluginRequest {
            source_path: source.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    settings
        .update_settings(w::UpdateSettingsRequest {
            expected_revision: 1,
            connections: vec![w::ConnectionInput {
                id: "c1".into(),
                name: "Fixture".into(),
                enabled: true,
                kind: w::ConnectionKind::CONNECTION_KIND_LLM.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Llm(Box::new(
                        w::LlmConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "openrouter".into(),
                                endpoint: format!("{origin}/v1"),
                                ..Default::default()
                            })
                            .into(),
                            model_ids: vec!["openai/gpt-4o-mini".into()],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .unwrap();
    register_credential(settings).await;
    w::ModelAssignment {
        connection_id: "c1".into(),
        model_id: "openai/gpt-4o-mini".into(),
        ..Default::default()
    }
}

async fn enable(settings: &w::SettingsServiceClient<HttpClient>, enabled: bool) {
    settings
        .set_plugin_enabled(w::SetPluginEnabledRequest {
            plugin_id: "openrouter".into(),
            enabled,
            ..Default::default()
        })
        .await
        .unwrap();
}

async fn wait_for_reply(chat: &w::ChatServiceClient<HttpClient>, companion: &str) {
    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let history = chat
                .get_history(w::HistoryRequest {
                    companion_id: companion.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            if history
                .view()
                .items
                .iter()
                .any(|msg| msg.text == "Fixture reply")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the guest response reaches the companion transcript");
}

#[tokio::test]
async fn a_plugin_bound_model_stays_on_the_plugin_route_across_enablement() {
    let (seen, mut requests) = mpsc::unbounded_channel();
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", upstream.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(answer))
        .with_state(seen);
    let server = tokio::spawn(async move { axum::serve(upstream, router).await.unwrap() });
    let source = tempfile::tempdir().unwrap();
    fixture(source.path(), &origin);

    let root = tempfile::tempdir().unwrap();
    let run = root.path().join("run");
    let mut child = spawn(&root.path().join("state"), &run);
    let (endpoint, token) = discovery(&run).await;
    let (transport, config) = w::connect(&endpoint, &token).unwrap();
    let host = w::HostServiceClient::new(transport.clone(), config.clone());
    let settings = w::SettingsServiceClient::new(transport.clone(), config.clone());
    let chat = w::ChatServiceClient::new(transport, config);
    let assignment = configure(&settings, source.path(), &origin).await;
    enable(&settings, false).await;
    let companion = host
        .create_companion(w::CreateCompanionRequest {
            name: "Fixture".into(),
            mind: "kind".into(),
            assignments: Some(w::ModelAssignments {
                companion: Some(assignment.clone()).into(),
                task: Some(assignment).into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .view()
        .id
        .to_owned();
    let message = |key: &str| w::SendMessageRequest {
        companion_id: companion.clone(),
        request_key: key.into(),
        text: "Synthetic prompt".into(),
        ..Default::default()
    };

    assert!(chat.send_message(message("disabled")).await.is_err());
    assert!(requests.try_recv().is_err());
    enable(&settings, true).await;
    chat.send_message(message("enabled")).await.unwrap();
    wait_for_reply(&chat, &companion).await;
    assert_eq!(requests.try_recv().unwrap(), (true, false));
    enable(&settings, false).await;
    assert!(chat.send_message(message("revoked")).await.is_err());
    settings
        .remove_plugin(w::RemovePluginRequest {
            plugin_id: "openrouter".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(chat.send_message(message("removed")).await.is_err());
    assert!(requests.try_recv().is_err());

    stop(&mut child).await;
    server.abort();
}
