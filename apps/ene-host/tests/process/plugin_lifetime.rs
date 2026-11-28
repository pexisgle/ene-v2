use super::{discovery, seed_companion, spawn, stop};
use connectrpc::client::HttpClient;
use ene_rpc as w;
use std::{os::unix::fs::PermissionsExt as _, path::Path, time::Duration};

fn reference_package(source: &Path) {
    let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/wasm32-unknown-unknown/release/ene_reference_wasm.wasm");
    std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(wasm, source.join("main.wasm")).unwrap();
    std::fs::write(source.join("manifest.json"), serde_json::json!({
        "contract_version": ene_plugin::CONTRACT_VERSION,
        "id": "reference", "name": "Reference", "version": "1",
        "entrypoints": [{"id": "main", "target": "host", "file": "main.wasm", "provides": ["harness"]}]
    }).to_string()).unwrap();
}

async fn assign_reference(
    host: &w::HostServiceClient<HttpClient>,
    settings: &w::SettingsServiceClient<HttpClient>,
) -> String {
    let id = seed_companion(
        settings,
        host,
        &w::ModelAssignment {
            connection_id: "c1".into(),
            model_id: "gpt-4o-mini".into(),
            ..Default::default()
        },
    )
    .await;
    let current = settings
        .get_settings(w::Empty::default())
        .await
        .unwrap()
        .view()
        .revision;
    settings
        .update_settings(w::UpdateSettingsRequest {
            expected_revision: current,
            connections: vec![w::ConnectionInput {
                id: "hand".into(),
                name: "Reference harness".into(),
                enabled: true,
                kind: w::ConnectionKind::CONNECTION_KIND_ACP.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Acp(Box::new(
                        w::AcpConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "reference".into(),
                                ..Default::default()
                            })
                            .into(),
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
    let revision = host
        .get_companion(w::Id {
            id: id.clone(),
            ..Default::default()
        })
        .await
        .unwrap()
        .view()
        .revision;
    host.update_companion(w::UpdateCompanionRequest {
        id: id.clone(),
        revision,
        task_assignment: Some(w::AssignmentPatch {
            change: Some(w::assignment_patch::Change::Set(Box::new(
                w::ModelAssignment {
                    connection_id: "hand".into(),
                    model_id: "reference".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    })
    .await
    .unwrap();
    id
}

async fn pending_request(work: &w::WorkServiceClient<HttpClient>, id: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let detail = work
                .get_work(w::WorkRequest {
                    work_id: id.into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            if let Some(run) = detail.view().runs.iter().next()
                && run.state.as_known() == Some(w::RunStatus::RUN_STATUS_WAITING_INPUT)
            {
                return run.pending_request_id.unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the installed harness reaches its approval")
}

async fn check_hand(settings: &w::SettingsServiceClient<HttpClient>) -> w::ConnectionCheck {
    settings
        .check_connection(w::CheckRequest {
            id: "hand".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_owned()
        .check
        .into_option()
        .unwrap()
}

async fn saved_hand_check(
    settings: &w::SettingsServiceClient<HttpClient>,
) -> Option<w::ConnectionCheck> {
    settings
        .get_settings(w::Empty::default())
        .await
        .unwrap()
        .into_owned()
        .connections
        .into_iter()
        .find(|connection| connection.id == "hand")
        .unwrap()
        .last_check
        .into_option()
}

async fn external_agent_is_explicitly_unavailable(settings: &w::SettingsServiceClient<HttpClient>) {
    let source = tempfile::tempdir().unwrap();
    std::fs::set_permissions(source.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let marker = source.path().join("external-agent-ran");
    std::fs::write(
        source.path().join("manifest.json"),
        serde_json::json!({
            "contract_version": ene_plugin::CONTRACT_VERSION,
            "id": "reference", "name": "External fixture", "version": "1",
            "entrypoints": [{
                "id": "main",
                "target": "acp",
                "command": ["touch", marker],
                "provides": ["harness"]
            }]
        })
        .to_string(),
    )
    .unwrap();
    settings
        .install_plugin(w::InstallPluginRequest {
            source_path: source.path().display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let check = check_hand(settings).await;
    assert!(!check.healthy);
    assert!(
        check
            .failure
            .into_option()
            .unwrap()
            .message
            .contains("外部 ACP")
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn disabling_a_live_harness_survives_restart_and_expires_its_approval() {
    let temp = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    reference_package(source.path());
    let state = temp.path().join("state");
    let runtime = temp.path().join("runtime");
    let mut child = spawn(&state, &runtime);
    let (endpoint, token) = discovery(&runtime).await;
    let (transport, config) = w::connect(&endpoint, &token).unwrap();
    let host = w::HostServiceClient::new(transport.clone(), config.clone());
    let settings = w::SettingsServiceClient::new(transport.clone(), config.clone());
    let work = w::WorkServiceClient::new(transport, config);
    let companion = assign_reference(&host, &settings).await;
    assert!(!check_hand(&settings).await.healthy);
    external_agent_is_explicitly_unavailable(&settings).await;

    let install = w::InstallPluginRequest {
        source_path: source.path().display().to_string(),
        ..Default::default()
    };
    settings.install_plugin(install.clone()).await.unwrap();
    let check = check_hand(&settings).await;
    assert!(check.healthy, "{check:?}");
    assert_eq!(check.capabilities, ["harness"]);
    assert!(saved_hand_check(&settings).await.unwrap().healthy);
    settings.install_plugin(install).await.unwrap();
    assert_eq!(saved_hand_check(&settings).await, None);
    assert!(check_hand(&settings).await.healthy);
    let start = w::StartWorkRequest {
        companion_id: companion,
        instruction: "Inspect the reference fixture".into(),
        ..Default::default()
    };
    let id = work
        .start_work(start.clone())
        .await
        .unwrap()
        .view()
        .id
        .to_owned();
    let request_id = pending_request(&work, &id).await;

    let result = settings
        .set_plugin_enabled(w::SetPluginEnabledRequest {
            plugin_id: "reference".into(),
            enabled: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(!result.view().enabled);
    assert_eq!(saved_hand_check(&settings).await, None);
    assert!(!check_hand(&settings).await.healthy);
    let detail = work
        .get_work(w::WorkRequest {
            work_id: id,
            ..Default::default()
        })
        .await
        .unwrap();
    let run = detail.view().runs.iter().next().unwrap();
    assert_eq!(run.state.as_known(), Some(w::RunStatus::RUN_STATUS_FAILED));
    assert!(run.pending_request_id.is_none());
    assert!(
        settings
            .submit_approval(w::SubmitApprovalRequest {
                request_id,
                decision: w::ApprovalDecision::APPROVAL_DECISION_APPROVE.into(),
                ..Default::default()
            })
            .await
            .is_err()
    );
    assert!(work.start_work(start.clone()).await.is_err());
    stop(&mut child).await;

    unavailable_executor_is_not_recovered_as_rig(&state, &runtime, start).await;
}

async fn unavailable_executor_is_not_recovered_as_rig(
    state: &Path,
    runtime: &Path,
    start: w::StartWorkRequest,
) {
    let mut child = spawn(state, runtime);
    let (endpoint, token) = discovery(runtime).await;
    let (transport, config) = w::connect(&endpoint, &token).unwrap();
    let settings = w::SettingsServiceClient::new(transport.clone(), config.clone());
    let work = w::WorkServiceClient::new(transport, config);
    assert!(work.start_work(start.clone()).await.is_err());
    settings
        .set_plugin_enabled(w::SetPluginEnabledRequest {
            plugin_id: "reference".into(),
            enabled: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let work_id = work.start_work(start).await.unwrap().view().id.to_owned();
    pending_request(&work, &work_id).await;
    let input = w::SendWorkRequest {
        work_id: work_id.clone(),
        request_key: "follow-up-before-restart".into(),
        text: "Use the alternate sample".into(),
        ..Default::default()
    };
    let accepted = work.send_work(input.clone()).await.unwrap().into_owned();
    let accepted_run = accepted.run_id.as_str();
    assert_ne!(accepted_run, "");
    child.signal(9).unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!exit.success());
    std::fs::remove_file(runtime.join("connection.json")).unwrap();
    std::fs::rename(
        state.join("plugins/.managed/reference"),
        state.join("reference-offline"),
    )
    .unwrap();

    let mut child = spawn(state, runtime);
    let (endpoint, token) = discovery(runtime).await;
    let (transport, config) = w::connect(&endpoint, &token).unwrap();
    let work = w::WorkServiceClient::new(transport, config);
    let retry = work.send_work(input.clone()).await.unwrap();
    assert_eq!(retry.view().request_id, accepted.request_id);
    assert_eq!(retry.view().run_id, accepted_run);
    let conflict = work
        .send_work(w::SendWorkRequest {
            text: "Different input".into(),
            ..input
        })
        .await
        .unwrap_err();
    assert_eq!(conflict.code, connectrpc::ErrorCode::AlreadyExists);
    let detail = work
        .get_work(w::WorkRequest {
            work_id,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_interrupted_work(detail.view(), accepted_run);
    stop(&mut child).await;
}

fn assert_interrupted_work(detail: &w::WorkViewView<'_>, accepted_run: &str) {
    assert_eq!(detail.runs.iter().count(), 2);
    assert!(detail.runs.iter().all(|run| {
        run.state.as_known() == Some(w::RunStatus::RUN_STATUS_FAILED)
            && run.pending_request_id.is_none()
    }));
    let queued = detail
        .runs
        .iter()
        .find(|run| run.run_id == accepted_run)
        .unwrap();
    assert_eq!(queued.failure.code, "work_input_interrupted");
}
