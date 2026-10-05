use super::*;
use ene_core::ports::{Credentials, Repository};
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
    fn environment(&self, _: &str) -> ene_core::ports::Result<String> {
        Ok("fixture-key".into())
    }
    fn session_only(&self) -> bool {
        false
    }
    fn load(
        &self,
        reference: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<String>> {
        self.stored.load(reference)
    }
    fn save(
        &self,
        reference: String,
        value: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async move {
            if self.saves.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ene_core::domain::Error::unavailable())
            } else {
                self.stored.save(reference, value).await
            }
        })
    }
    fn forget(&self, reference: String) {
        self.stored.forget(reference);
    }
}

#[tokio::test]
async fn same_key_retries_storage_and_honors_environment_lifetime() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let credentials = Arc::new(RecoveringCredentials::default());
    let app = App::new(
        ene_core::application::HostApplication::new(
            Arc::new(store.clone()),
            Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
            credentials.clone(),
        ),
        "private-token".into(),
    );
    let (status, settings) = mutate(&app, "PATCH", "/v1/settings", serde_json::json!({"revision":1,"connections":[{"kind":"llm","adapter":"openai","endpoint":"https://api.openai.com/v1","options":{}}]}), None).await;
    assert_eq!(status, StatusCode::OK);
    let id = settings["connections"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = format!("/v1/credentials/{id}");
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
    let restarted = ene_core::application::HostApplication::new(
        Arc::new(store),
        Arc::new(crate::infrastructure::llm::registry::Registry::discover()),
        credentials,
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
