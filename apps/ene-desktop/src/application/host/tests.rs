use super::super::*;
use crate::infrastructure::host::{Command, Controller, Event};
use serde_json::json;

struct Headless;
impl slint::platform::Platform for Headless {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        Ok(
            slint::platform::software_renderer::MinimalSoftwareWindow::new(
                slint::platform::software_renderer::RepaintBufferType::NewBuffer,
            ),
        )
    }
}

#[test]
fn notifications_preserve_displayed_drafts_and_save_their_original_revisions() {
    slint::platform::set_platform(Box::new(Headless)).unwrap();
    let ui = AppWindow::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let app = Application::bind(
        &ui,
        Mode::Demo,
        temp.path().join("client.json"),
        settings::ClientSettings::default(),
        None,
        None,
    );
    let (controller, mut requests) = Controller::test_channel();
    app.host.replace(Some(controller));
    let state = ui.global::<UiState>();
    state.set_host_connected(true);
    let mut provider: ene_api_types::provider::ProviderInfo = serde_json::from_value(json!({
        "id":"fixture","name":"Fixture","kind":"plugin","endpoint":"https://old.test/v1","options":{},"settings":[],
        "requires_credentials":false,"environment_variable":null,"destinations":["https://old.test"],"state":"ready","error":null,"revision":"v1","capabilities":{"streaming_text":true,"model_list":true}
    })).unwrap();
    let mut snapshot: ene_api_types::Snapshot = serde_json::from_value(json!({
        "host_id":"h","environment_id":"e","capabilities":{},"cursor":{"epoch":"e","seq":0},
        "companions":[{"id":"c","name":"ene","mind":"old mind","body":null,"voice":null,"model_assignments":{},"availability":{},"revision":1}],
        "messages":[],"message_cursors":{},"tasks":[],"questions":[],"grants":[],
        "settings":{"revision":1,"connections":[{"id":"p","kind":"llm","adapter":"fixture","endpoint":"https://old.test/v1","options":{},"credential_state":"missing","capabilities":null,"last_check":null}]}
    })).unwrap();
    app.handle_host_event(&ui, Event::Providers(vec![provider.clone()]));
    app.handle_host_event(&ui, Event::View(Box::new(snapshot.clone()), vec![]));
    state.set_companion_name("draft name".into());
    state.set_companion_dirty(true);
    state.set_provider_endpoint("https://draft.test/v1".into());
    state.set_connection_dirty(true);
    state.set_allow_conversation(true);
    state.set_permissions_dirty(true);
    provider.revision = "v2".into();
    provider
        .destinations
        .push("https://undisplayed.test".into());
    snapshot.settings.revision = 2;
    snapshot.companions[0].revision = 2;
    snapshot.companions[0].mind = "remote mind".into();
    snapshot.settings.connections[0].endpoint = "https://remote.test/v1".into();
    app.handle_host_event(&ui, Event::Providers(vec![provider]));
    app.handle_host_event(&ui, Event::View(Box::new(snapshot), vec![]));
    assert_eq!(state.get_destinations(), "https://old.test");
    assert_eq!(state.get_companion_mind(), "old mind");
    assert_eq!(state.get_provider_endpoint(), "https://draft.test/v1");

    state.invoke_save_companion();
    match requests.try_recv().unwrap().command {
        Command::Companion {
            revision,
            name,
            mind,
        } => {
            assert_eq!(revision, 1);
            assert_eq!(name, "draft name");
            assert_eq!(mind, "old mind");
        }
        _ => panic!("expected companion save"),
    }
    state.set_host_operation(0);
    state.invoke_configure_host();
    match requests.try_recv().unwrap().command {
        Command::Configure {
            settings_revision,
            companion_revision,
            endpoint,
            ..
        } => {
            assert_eq!(settings_revision, 1);
            assert_eq!(companion_revision, 1);
            assert_eq!(endpoint, "https://draft.test/v1");
        }
        _ => panic!("expected connection save"),
    }
    state.set_host_operation(0);
    state.invoke_save_permissions();
    match requests.try_recv().unwrap().command {
        Command::Permission {
            consent,
            destinations,
            ..
        } => {
            assert_eq!(consent.settings_revision, 1);
            assert_eq!(consent.provider_revision, "v1");
            assert_eq!(destinations, vec!["https://old.test"]);
        }
        _ => panic!("expected permission save"),
    }
    state.set_host_operation(0);
    state.set_category(1);
    app.reset_host_page(&ui);
    assert_eq!(state.get_companion_mind(), "remote mind");
    state.invoke_save_companion();
    match requests.try_recv().unwrap().command {
        Command::Companion { revision, .. } => assert_eq!(revision, 2),
        _ => panic!("expected refreshed companion save"),
    }
    state.set_host_operation(0);
    state.set_category(2);
    app.reset_host_page(&ui);
    assert_eq!(state.get_provider_endpoint(), "https://remote.test/v1");
    state.set_category(3);
    app.reset_host_page(&ui);
    assert_eq!(
        state.get_destinations(),
        "https://old.test → https://undisplayed.test"
    );
    state.invoke_save_permissions();
    match requests.try_recv().unwrap().command {
        Command::Permission {
            consent,
            destinations,
            ..
        } => {
            assert_eq!(consent.settings_revision, 2);
            assert_eq!(consent.provider_revision, "v2");
            assert_eq!(
                destinations,
                vec!["https://old.test", "https://undisplayed.test"]
            );
        }
        _ => panic!("expected reconfirmed permission save"),
    }
}
