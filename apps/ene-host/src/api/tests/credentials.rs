use super::*;
use crate::ports::{Credentials, Repository};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct RecoveringCredentials {
    stored: StoredCredentials,
    saves: AtomicUsize,
}
impl Credentials for RecoveringCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        Ok("fixture-key".into())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        self.stored.load(reference)
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            if self.saves.fetch_add(1, Ordering::SeqCst) == 0 {
                self.stored.save(reference, value).await?;
                Err(crate::domain::Error::unavailable())
            } else {
                self.stored.save(reference, value).await
            }
        })
    }
    fn forget(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.stored.forget(reference)
    }
}

#[tokio::test]
async fn same_key_retries_storage_and_honors_environment_lifetime() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let credentials = Arc::new(RecoveringCredentials::default());
    let shutdown = tokio::sync::watch::channel(false).0;
    let app = App::new(
        crate::application::HostApplication::new(
            Arc::new(store.clone()),
            Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
            credentials.clone(),
            shutdown.clone(),
        ),
        "private-token".into(),
        shutdown,
    );
    let (status, settings) = mutate(&app, "PATCH", "Settings", serde_json::json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}), None).await;
    assert_eq!(status, StatusCode::OK);
    let id = settings["connections"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("Credentials/{id}");
    for (expected, saves) in [("session_only", 1), ("stored", 2), ("stored", 2)] {
        let (status, result) = mutate(
            &app,
            "PUT",
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
        "PUT",
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
    assert!(credentials.stored.0.lock().unwrap().is_empty());
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
        "PUT",
        &path,
        serde_json::json!({"secret":"fixture-key"}),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["credential_state"], "stored");
    assert_eq!(credentials.saves.load(Ordering::SeqCst), 3);
    app.application.stop().await.unwrap();
    let restarted = crate::application::HostApplication::new(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials,
        tokio::sync::watch::channel(false).0,
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

struct SlowLoadCredentials {
    stored: StoredCredentials,
    entered: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}
impl Credentials for SlowLoadCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        Ok("fixture".into())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        Box::pin(async move {
            let old = self.stored.load(reference).await?;
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(old)
        })
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.stored.save(reference, value)
    }
    fn forget(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.stored.forget(reference)
    }
}
#[tokio::test]
async fn slow_startup_load_does_not_block_revocation_or_restore_its_retired_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("load.db")).await.unwrap();
    let credentials = Arc::new(SlowLoadCredentials {
        stored: StoredCredentials::default(),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
    });
    let original = crate::application::HostApplication::new(
        Arc::new(store.clone()),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
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
    let restarted = crate::application::HostApplication::new(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
    );
    let initializing = restarted.clone();
    let loading = tokio::spawn(async move { initializing.initialize().await });
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        credentials.entered.acquire(),
    )
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
    credentials.release.add_permits(1);
    loading.await.unwrap().unwrap();
    assert_eq!(
        restarted.settings().await.unwrap().connections[0].credential_state,
        "missing"
    );
    restarted.stop().await.unwrap();
    assert!(credentials.stored.0.lock().unwrap().is_empty());
}

#[derive(Default)]
struct UnresponsiveCleanupCredentials {
    stored: StoredCredentials,
    attempts: AtomicUsize,
}
impl Credentials for UnresponsiveCleanupCredentials {
    fn environment(&self, _: &str) -> crate::ports::Result<String> {
        Ok("fixture".into())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<String>> {
        self.stored.load(reference)
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        self.stored.save(reference, value)
    }
    fn forget(&self, _: String) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        })
    }
}

#[tokio::test(start_paused = true)]
async fn unresponsive_cleanup_is_bounded_visible_and_does_not_report_successful_shutdown() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("cleanup.db")).await.unwrap();
    let credentials = Arc::new(UnresponsiveCleanupCredentials::default());
    let app = crate::application::HostApplication::new(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials.clone(),
        tokio::sync::watch::channel(false).0,
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
    assert_eq!(credentials.attempts.load(Ordering::SeqCst), 4);
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
    assert_eq!(credentials.stored.0.lock().unwrap().len(), 1);
}
