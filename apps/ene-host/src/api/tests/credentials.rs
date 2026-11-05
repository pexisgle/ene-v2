use super::*;
use std::sync::{Arc, atomic::Ordering};

#[tokio::test]
async fn same_key_retries_storage_and_honors_environment_lifetime() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let credentials = Arc::new(StoredCredentials {
        environment: Some("fixture-key"),
        fail_first_save: true,
        ..Default::default()
    });
    let app = super::host_app(
        store.clone(),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
    );
    let (status, settings) = mutate(&app, "UpdateSettings", serde_json::json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}), None).await;
    assert_eq!(status, StatusCode::OK);
    let id = settings["connections"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("RegisterCredentials/{id}");
    for (expected, saves) in [("session_only", 1), ("stored", 2), ("stored", 2)] {
        let (status, result) = mutate(
            &app,
            &path,
            serde_json::json!({"secret":"fixture-key"}),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["credential_state"], expected);
        assert_eq!(credentials.saves.load(Ordering::SeqCst), saves);
    }
    let (status, result) = mutate(
        &app,
        &path,
        serde_json::json!({"source":"environment"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["credential_state"], "session_only");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while app
            .application
            .snapshot(None)
            .await
            .unwrap()
            .health
            .credential_cleanup_pending
            != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(credentials.values.lock().unwrap().is_empty());
    assert!(
        !store
            .configuration()
            .await
            .unwrap()
            .credential_refs
            .contains_key(&id)
    );
    let (status, result) = mutate(
        &app,
        &path,
        serde_json::json!({"secret":"fixture-key"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["credential_state"], "stored");
    assert_eq!(credentials.saves.load(Ordering::SeqCst), 3);
    app.application.stop().await.unwrap();
    let restarted = crate::application::HostApplication::with_plugins(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials,
        tokio::sync::watch::channel(false).0,
        None,
    );
    restarted.initialize().await.unwrap();
    assert_eq!(
        restarted
            .snapshot(None)
            .await
            .unwrap()
            .stored
            .settings
            .connections[0]
            .credential_state,
        "stored"
    );
    restarted.stop().await.unwrap();
}

#[tokio::test]
async fn slow_startup_load_does_not_block_revocation_or_restore_its_retired_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("load.db")).await.unwrap();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let credentials = Arc::new(StoredCredentials {
        environment: Some("fixture"),
        load_gate: Some((entered.clone(), release.clone())),
        ..Default::default()
    });
    let original = crate::application::HostApplication::with_plugins(
        Arc::new(store.clone()),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
        None,
    );
    let settings = original
        .patch_settings(crate::application::SettingsPatch {
            revision: 1,
            connections: ene_rpc::ConnectionInputs {
                items: vec![crate::application::ConnectionInput {
                    id: None,
                    create: false,
                    kind: "llm".into(),
                    adapter: "openai".into(),
                    endpoint: "https://api.openai.com/v1".into(),
                    options: ene_rpc::JsonDocument {
                        json: "{}".into(),
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let connection = settings.connections[0].id.clone();
    original
        .register_credentials(connection.clone(), Some("old".into()), None)
        .await
        .unwrap();
    original.stop().await.unwrap();
    let restarted = crate::application::HostApplication::with_plugins(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
        None,
    );
    let initializing = restarted.clone();
    let loading = tokio::spawn(async move { initializing.initialize().await });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        restarted.revoke_credentials(connection),
    )
    .await
    .unwrap()
    .unwrap();
    release.add_permits(1);
    loading.await.unwrap().unwrap();
    assert_eq!(
        restarted.settings().await.unwrap().connections[0].credential_state,
        "missing"
    );
    restarted.stop().await.unwrap();
    assert!(credentials.values.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn unresponsive_cleanup_is_bounded_visible_and_does_not_report_successful_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("cleanup.db")).await.unwrap();
    let credentials = Arc::new(StoredCredentials {
        environment: Some("fixture"),
        unresponsive_forget: true,
        ..Default::default()
    });
    let app = crate::application::HostApplication::with_plugins(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
        None,
    );
    let settings = app
        .patch_settings(crate::application::SettingsPatch {
            revision: 1,
            connections: ene_rpc::ConnectionInputs {
                items: vec![crate::application::ConnectionInput {
                    id: None,
                    create: false,
                    kind: "llm".into(),
                    adapter: "openai".into(),
                    endpoint: "https://api.openai.com/v1".into(),
                    options: ene_rpc::JsonDocument {
                        json: "{}".into(),
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let id = settings.connections[0].id.clone();
    app.register_credentials(id.clone(), Some("fixture".into()), None)
        .await
        .unwrap();
    app.revoke_credentials(id).await.unwrap();
    let error = app.stop().await.unwrap_err();
    assert_eq!(error.reason, Some("credential_cleanup_failed"));
    assert_eq!(credentials.forgets.load(Ordering::SeqCst), 4);
    assert_eq!(
        app.snapshot(None)
            .await
            .unwrap()
            .health
            .credential_cleanup_pending,
        1
    );
    assert_eq!(
        app.settings().await.unwrap().connections[0].credential_state,
        "missing"
    );
    assert_eq!(credentials.values.lock().unwrap().len(), 1);
}
