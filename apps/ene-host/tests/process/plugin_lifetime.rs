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

    settings
        .install_plugin(w::InstallPluginRequest {
            source_path: source.path().display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
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
    let accepted = work
        .send_work(input.clone())
        .await
        .unwrap()
        .view()
        .request_id
        .to_owned();
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
    assert_eq!(retry.view().request_id, accepted);
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
    assert_eq!(detail.view().runs.iter().count(), 1);
    let run = detail.view().runs.iter().next().unwrap();
    assert_eq!(run.state.as_known(), Some(w::RunStatus::RUN_STATUS_FAILED));
    assert!(run.pending_request_id.is_none());
    stop(&mut child).await;
}
