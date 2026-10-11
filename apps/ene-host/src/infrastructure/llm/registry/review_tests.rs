use super::*;
use crate::infrastructure::{credentials, sqlite::Store};
use ene_core::{application::HostApplication, domain::*, ports::Repository};
use serde_json::json;
use std::{path::Path, sync::Arc};

fn package(root: &Path, name: &str, id: &str) -> PathBuf {
    let path = root.join(name);
    crate::test_support::create_directory(&path).unwrap();
    let mut manifest: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../../providers/openrouter/manifest.json"
    ))
    .unwrap();
    manifest["id"] = json!(id);
    crate::test_support::write(path.join("manifest.json"), manifest.to_string()).unwrap();
    path
}
fn connection() -> Connection {
    Connection {
        id: "p".into(),
        kind: "llm".into(),
        adapter: "valid".into(),
        endpoint: "https://openrouter.ai/api/v1".into(),
        options: json!({"upstream":"openai","allow_fallbacks":false}),
        credential_state: "missing".into(),
        capabilities: None,
        last_check: None,
    }
}
fn application(store: Store, registry: Registry) -> HostApplication {
    HostApplication::new(
        Arc::new(store),
        Arc::new(registry),
        Arc::new(credentials::Store::default()),
        tokio::sync::watch::channel(false).0,
    )
}

#[test]
fn discovery_isolates_invalid_and_duplicate_packages() {
    let root = crate::test_support::package_directory();
    package(root.path(), "healthy", "valid");
    crate::test_support::create_directory(root.path().join("broken")).unwrap();
    package(root.path(), "duplicate-a", "duplicate");
    package(root.path(), "duplicate-b", "duplicate");
    package(root.path(), "duplicate-c", "duplicate");
    let registry = Registry::at(root.path().into());
    let descriptors = registry.descriptors();
    assert!(
        descriptors
            .iter()
            .any(|d| d.id == "valid" && d.state == "disabled")
    );
    assert!(
        descriptors
            .iter()
            .any(|d| d.name == "broken" && d.error.is_some())
    );
    assert_eq!(
        descriptors.iter().filter(|d| d.id == "duplicate").count(),
        1
    );
    assert!(
        descriptors
            .iter()
            .any(|d| d.id == "duplicate" && d.state == "failed")
    );
    assert!(
        descriptors
            .iter()
            .find(|d| d.id == openai::ID)
            .unwrap()
            .error
            .is_none()
    );
    assert!(registry.policy(&connection()).is_ok());
    let mut duplicate = connection();
    duplicate.adapter = "duplicate".into();
    assert!(registry.policy(&duplicate).is_err());
    assert!(registry.resolve(&connection()).is_err());
    let before = registry.scope_fingerprint(&connection()).unwrap();
    registry
        .entries
        .write()
        .unwrap()
        .get_mut("valid")
        .unwrap()
        .state = State::Failed("startup failure".into());
    assert_eq!(registry.scope_fingerprint(&connection()).unwrap(), before);
    assert!(registry.policy(&connection()).is_ok());
    assert!(registry.resolve(&connection()).is_err());
}

#[tokio::test]
async fn persisted_grants_survive_restart_and_unavailable_policy_but_invalidate_known_changes() {
    let temp = crate::test_support::package_directory();
    let root = temp.path().join("providers");
    crate::test_support::create_directory(&root).unwrap();
    let path = package(&root, "healthy", "valid");
    let db = temp.path().join("host.sqlite3");
    let registry = Registry::at(root.clone());
    registry
        .entries
        .write()
        .unwrap()
        .get_mut("valid")
        .unwrap()
        .state = State::Ready(Capabilities::default());
    let store = Store::open(db.clone()).await.unwrap();
    let mut config = Repository::configuration(&store).await.unwrap();
    let p = connection();
    let policy = registry.policy(&p).unwrap();
    config.settings.connections.push(p.clone());
    let companion_id = config.companions.keys().next().unwrap().clone();
    config
        .companions
        .get_mut(&companion_id)
        .unwrap()
        .model_assignments
        .companion = Some(ModelAssignment {
        connection_id: p.id.clone(),
        model_id: "model".into(),
    });
    config.grants.push(Grant {
        id: "g".into(),
        companion_id: companion_id.clone(),
        kind: "data_transfer".into(),
        connection_id: p.id.clone(),
        destinations: policy.destinations,
        data_kinds: vec!["conversation".into()],
        purpose: GrantPurpose::Conversation,
        status: GrantStatus::Active,
        revision: 1,
        invalidation_reason: None,
        connection_fingerprint: policy.fingerprint,
    });
    Repository::commit_configuration(&store, ene_core::ports::ConfigurationCommit::Update(config))
        .await
        .unwrap();
    drop(store);
    drop(registry);

    // A new host discovers the same implementation disabled, without losing consent.
    let store = Store::open(db).await.unwrap();
    let app = application(store.clone(), Registry::at(root.clone()));
    app.initialize().await.unwrap();
    let saved = Repository::configuration(&store).await.unwrap();
    assert_eq!(saved.grants[0].status, GrantStatus::Active);
    assert_eq!(saved.grants[0].revision, 1);
    assert_eq!(
        saved.companions[&companion_id].availability["chat"]["reasons"][0]["code"],
        "provider_unavailable"
    );
    app.reload_providers().await.unwrap();
    assert_eq!(
        Repository::configuration(&store).await.unwrap().grants[0].status,
        GrantStatus::Active
    );

    let manifest = std::fs::read(path.join("manifest.json")).unwrap();
    crate::test_support::write(path.join("manifest.json"), "broken").unwrap();
    app.reload_providers().await.unwrap();
    assert_eq!(
        Repository::configuration(&store).await.unwrap().grants[0].status,
        GrantStatus::Active
    );
    crate::test_support::write(path.join("manifest.json"), manifest).unwrap();
    crate::test_support::write(path.join("provider"), "changed implementation").unwrap();
    app.reload_providers().await.unwrap();
    let changed = Repository::configuration(&store).await.unwrap();
    assert_eq!(changed.grants[0].status, GrantStatus::Invalidated);
    assert_eq!(
        changed.grants[0].invalidation_reason.as_deref(),
        Some("provider_changed")
    );
    assert_eq!(changed.grants[0].revision, 2);

    // A verified manifest can remove the old destination. This is a real scope
    // change even though evaluating the old connection's executable policy fails.
    let mut config = changed;
    config.grants[0].status = GrantStatus::Active;
    Repository::commit_configuration(&store, ene_core::ports::ConfigurationCommit::Update(config))
        .await
        .unwrap();
    drop(app);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.join("manifest.json")).unwrap()).unwrap();
    manifest["endpoint"] = json!("https://other.example/v1");
    manifest["destinations"] = json!(["https://other.example"]);
    manifest["routing"] = json!({"type":"direct"});
    crate::test_support::write(path.join("manifest.json"), manifest.to_string()).unwrap();
    let registry = Registry::at(root);
    assert!(registry.policy(&p).is_err());
    assert!(registry.scope_fingerprint(&p).is_ok());
    let restarted = application(store.clone(), registry);
    restarted.initialize().await.unwrap();
    let saved = Repository::configuration(&store).await.unwrap();
    assert_eq!(saved.grants[0].status, GrantStatus::Invalidated);
    assert_eq!(saved.grants[0].revision, 3);
}
