use super::*;
#[tokio::test]
async fn clients_are_registered_and_activation_is_exclusive() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    let a = runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client-a".into(),
                    name: "A".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    let b = runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client-b".into(),
                    name: "B".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(a.revision, 1);
    assert_eq!(b.revision, 1);
    let first = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client-a".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(first.client_id.as_deref(), Some("client-a"));
    let second = runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client-b".into(),
                        expected_revision: first.revision,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    assert_eq!(second.client_id.as_deref(), Some("client-b"));
    assert!(
        runtime
            .call({
                let companion = companion.clone();
                let revision = second.revision + 1;
                move |app| {
                    client::activate(
                        app,
                        w::ActivationRequest {
                            companion_id: companion,
                            client_id: "client-a".into(),
                            expected_revision: revision,
                            ..Default::default()
                        },
                    )
                }
            })
            .await
            .is_err()
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_device_request_nobody_answers_becomes_uncertain() {
    let temp = tempfile::tempdir().unwrap();
    let parent = parent_model(vec![
        vec![
            E::tool_call(
                "device",
                "device_request",
                json!({"capability": "files", "input": {"path": "note.txt"}}),
            ),
            E::final_response_with_default_usage(),
        ],
        vec![E::text("after"), E::final_response_with_default_usage()],
    ]);
    let runtime = Runtime::start_with(
        temp.path(),
        Options {
            models: Some(Arc::new(ScriptedSource(Scripted {
                parent,
                worker: text_model("unused"),
                revisions: AtomicU64::new(0),
            }))),
            device_timeout: Duration::from_millis(30),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let companion = default_companion(&runtime).await;
    runtime
        .call(|app| {
            client::register(
                app,
                w::RegisterClientRequest {
                    id: "client".into(),
                    name: "client".into(),
                    platform: "linux".into(),
                    capabilities: vec!["files".into()],
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    runtime
        .call(|app| {
            client::set_capabilities(
                app,
                w::SetCapabilitiesRequest {
                    client_id: "client".into(),
                    capabilities: vec!["files".into()],
                    granted_capabilities: vec!["files".into()],
                    revision: 1,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();
    runtime
        .call({
            let companion = companion.clone();
            move |app| {
                client::activate(
                    app,
                    w::ActivationRequest {
                        companion_id: companion,
                        client_id: "client".into(),
                        expected_revision: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    send(&runtime, &companion, "device", "端末で確認して").await;
    until(&runtime, &companion, "after").await;
    let devices = runtime
        .call(|app| app.world().resource::<Store>().devices())
        .await
        .unwrap();
    assert_eq!(devices[0].status, client::DeviceStatus::Uncertain);
    runtime.shutdown().await.unwrap();
}
