use super::*;
use ene_core::{
    application::{ConfigurationCommand, ConfigurationTarget},
    domain::{Companion, Configuration, Error, ErrorKind},
    ports::llm::ProviderRegistry,
};

struct Rename {
    id: String,
    revision: u64,
    fail: bool,
}

struct InvalidTopology {
    companion: String,
    revision: u64,
    remove: bool,
}
impl ConfigurationCommand for InvalidTopology {
    type Output = ();
    fn target(&self) -> ConfigurationTarget {
        ConfigurationTarget::Settings {
            revision: self.revision,
        }
    }
    fn apply(&self, c: &mut Configuration, _: &dyn ProviderRegistry) -> Result<(), Error> {
        if self.remove {
            c.companions.remove(&self.companion);
        } else {
            c.companions
                .get_mut(&self.companion)
                .unwrap()
                .model_assignments
                .companion = Some(ene_api_types::ModelAssignment {
                connection_id: "missing".into(),
                model_id: "fixture".into(),
            });
        }
        Ok(())
    }
    fn output(&self, _: &Configuration) -> Result<(), Error> {
        Ok(())
    }
}

#[tokio::test]
async fn configuration_guard_rejects_unsupported_deletions_and_dangling_references() {
    let (app, _temp) = app().await;
    let snapshot = app.application.snapshot(None).await.unwrap();
    let companion = &snapshot.stored.companions[0];
    let mut events = app.application.subscribe().await;
    for remove in [true, false] {
        let error = app
            .application
            .configure(InvalidTopology {
                companion: companion.id.clone(),
                revision: snapshot.stored.settings.revision,
                remove,
            })
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            if remove {
                ErrorKind::UnsupportedCapability
            } else {
                ErrorKind::StateConflict
            }
        );
        let actual = app.application.snapshot(None).await.unwrap();
        assert_eq!(
            actual.stored.settings.revision,
            snapshot.stored.settings.revision
        );
        assert_eq!(
            actual.stored.companions[0].model_assignments,
            companion.model_assignments
        );
        assert!(matches!(
            events.receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
}
impl ConfigurationCommand for Rename {
    type Output = Companion;
    fn target(&self) -> ConfigurationTarget {
        ConfigurationTarget::Companion {
            id: self.id.clone(),
            revision: self.revision,
        }
    }
    fn apply(&self, c: &mut Configuration, _: &dyn ProviderRegistry) -> Result<(), Error> {
        let companion = c.companions.get_mut(&self.id).unwrap();
        companion.name = "framework rename".into();
        if self.fail {
            return Err(Error::invalid());
        }
        Ok(())
    }
    fn output(&self, c: &Configuration) -> Result<Companion, Error> {
        Ok(c.companions[&self.id].clone())
    }
}

#[tokio::test]
async fn configuration_command_rolls_back_validation_and_sql_failures_without_events() {
    let f = ConversationFixture::new().await;
    let original = f.app.application.companion(&f.companion).await.unwrap();
    let mut events = f.app.application.subscribe().await;
    for validation_failure in [true, false] {
        if !validation_failure {
            f.store.run(|db| { db.execute_batch("CREATE TEMP TRIGGER fail_config BEFORE UPDATE ON companions BEGIN SELECT RAISE(ABORT,'fixture'); END;")?; Ok(()) }).await.unwrap();
        }
        let failure = f
            .app
            .application
            .configure(Rename {
                id: f.companion.clone(),
                revision: original.revision,
                fail: validation_failure,
            })
            .await
            .unwrap_err();
        assert_eq!(
            failure.kind,
            if validation_failure {
                ErrorKind::InvalidRequest
            } else {
                ErrorKind::StorageFailure
            }
        );
        let actual = f.app.application.companion(&f.companion).await.unwrap();
        assert_eq!(actual.name, original.name);
        assert_eq!(actual.revision, original.revision);
        assert!(matches!(
            events.receiver.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
    }
    f.store
        .run(|db| {
            db.execute_batch("DROP TRIGGER fail_config")?;
            Ok(())
        })
        .await
        .unwrap();
    let saved = f
        .app
        .application
        .configure(Rename {
            id: f.companion.clone(),
            revision: original.revision,
            fail: false,
        })
        .await
        .unwrap();
    assert_eq!(saved.name, "framework rename");
    assert_eq!(saved.availability["chat"]["ready"], true);
    let event = events.receiver.recv().await.unwrap();
    assert_eq!(event.kind, "companion.updated");
    assert_eq!(event.companion_id.as_deref(), Some(f.companion.as_str()));
    assert_eq!(event.data["name"], saved.name);
    assert_eq!(
        f.app
            .application
            .snapshot(Some(&events.id))
            .await
            .unwrap()
            .cursor
            .seq,
        event.seq
    );
    assert!(matches!(
        events.receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn stop_drains_a_command_after_its_caller_disconnects() {
    let (app, _temp) = app().await;
    let (entered, admitted) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let owner = app.application.clone();
    let caller = tokio::spawn(async move {
        owner
            .execute(async move {
                entered.send(()).unwrap();
                gate.await.unwrap();
                Ok(())
            })
            .await
    });
    admitted.await.unwrap();
    caller.abort();
    let stop = app.application.stop();
    tokio::pin!(stop);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut stop)
            .await
            .is_err()
    );
    assert_eq!(
        app.application
            .execute(async { Ok(()) })
            .await
            .unwrap_err()
            .kind,
        ErrorKind::StateConflict
    );
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), stop)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn failed_check_receipt_commit_keeps_the_run_faulted_and_prevents_new_work() {
    let mut f = ConversationFixture::new().await;
    let (status, _) = mutate(
        &f.app,
        "POST",
        "/v1/grants",
        json!({
            "companion_id":f.companion,"connection_id":f.connection,"kind":"data_transfer",
            "consent":{"settings_revision":2,"provider_revision":"builtin-openai-v2-model-catalog"},
            "destinations":["https://api.openai.com","https://models.dev"],
            "data_kinds":["test_input"],"purpose":"connection_check"
        }),
        Some("check-permission"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    f.store.run(|db| { db.execute_batch("CREATE TEMP TRIGGER fail_receipt BEFORE UPDATE ON receipts BEGIN SELECT RAISE(ABORT,'fixture'); END;")?; Ok(()) }).await.unwrap();
    let mut events = f.app.application.subscribe().await;
    let app = f.app.application.clone();
    let connection = f.connection.clone();
    let companion = f.companion.clone();
    let check = tokio::spawn(async move {
        ene_core::application::operations::check(
            app,
            connection,
            "failed-check".into(),
            ene_api_types::CheckInput {
                companion_id: companion,
                purpose: "companion".into(),
                model_id: Some("fixture".into()),
            },
        )
        .await
    });
    f.started().await;
    f.finish.add_permits(1);
    assert_eq!(
        check.await.unwrap().unwrap_err().kind,
        ErrorKind::StorageFailure
    );
    let failure = events.receiver.recv().await.unwrap();
    assert_eq!(failure.kind, "generation.persistence_failed");
    assert!(failure.data.get("request_id").is_some());
    assert!(matches!(
        events.receiver.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let view = f.app.application.snapshot(None).await.unwrap();
    assert_eq!(
        view.stored.companions[0].availability["chat"]["ready"],
        false
    );
    assert!(view.stored.settings.connections[0].last_check.is_none());
    assert_eq!(
        mutate(
            &f.app,
            "POST",
            &format!("/v1/companions/{}/messages", f.companion),
            json!({"text":"next"}),
            Some("next")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        f.app.application.stop().await.unwrap_err().kind,
        ErrorKind::StorageFailure
    );
}
