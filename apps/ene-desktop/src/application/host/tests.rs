use super::super::*;
use crate::infrastructure::host::{
    ActionEvent, ActionResultEnvelope, Command, Controller, Event, Request,
};
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
    // Host integration retains the live local settings contract inherited from #4.
    let mut appearance = state.get_settings_draft();
    appearance.size_text = "2".into();
    appearance.theme_index = 2;
    state.set_settings_draft(appearance);
    state.invoke_settings_edited();
    assert_eq!(app.settings.borrow().saved.theme_index, 2);
    assert_eq!(app.settings.borrow().saved.avatar.config.size_percent, 100);
    assert!(!state.get_size_error().is_empty());
    assert_eq!(state.get_settings_draft().size_text, "2");
    let mut appearance = state.get_settings_draft();
    appearance.size_text = "170".into();
    appearance.font_family = "monospace".into();
    state.set_settings_draft(appearance);
    state.invoke_settings_edited();
    assert_eq!(app.settings.borrow().saved.avatar.config.size_percent, 170);
    assert_eq!(state.get_font_family(), "monospace");
    assert!(state.get_size_error().is_empty());
    assert!(!state.get_settings_dirty());
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
    // A failed background request remains retryable without a redraw retry loop.
    let failed = requests.try_recv().unwrap().context.unwrap();
    app.refresh_availability(&ui);
    assert!(
        requests.try_recv().is_err(),
        "duplicate request while pending"
    );
    app.handle_host_event(
        &ui,
        Event::Action(Box::new(ActionResultEnvelope {
            context: failed.clone(),
            payload: ActionEvent::Error("temporary failure".into()),
        })),
    );
    assert_eq!(state.get_availability_state(), 3);
    assert!(app.availability.borrow().is_none());
    assert!(app.host_drafts.borrow().permission.is_none());
    app.refresh_availability(&ui);
    assert!(
        requests.try_recv().is_err(),
        "failure must wait for explicit retry"
    );
    state.invoke_retry_availability();
    assert_eq!(state.get_availability_state(), 1);
    let retry = requests.try_recv().unwrap().context.unwrap();
    assert_ne!(retry.id, failed.id);
    app.handle_host_event(
        &ui,
        Event::Action(Box::new(ActionResultEnvelope {
            context: failed,
            payload: ActionEvent::Error("late failure".into()),
        })),
    );
    assert_eq!(
        state.get_availability_state(),
        1,
        "stale errors cannot clear the retry"
    );
    app.handle_host_event(
        &ui,
        Event::Action(Box::new(ActionResultEnvelope {
            context: retry,
            payload: ActionEvent::Error("temporary failure again".into()),
        })),
    );
    state.set_category(3);
    app.reset_host_page(&ui);
    assert_eq!(state.get_availability_state(), 1);
    respond_availability(&app, &ui, &mut requests, 1, "v1", vec!["https://old.test"]);
    assert_eq!(state.get_availability_state(), 2);
    assert!(app.host_drafts.borrow().permission.is_some());
    state.set_companion_name("draft name".into());
    state.invoke_host_draft_edited(1);
    state.set_provider_endpoint("https://draft.test/v1".into());
    state.invoke_host_draft_edited(0);
    state.set_allow_conversation(true);
    state.invoke_host_draft_edited(2);
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
    respond_availability(
        &app,
        &ui,
        &mut requests,
        2,
        "v2",
        vec!["https://old.test", "https://undisplayed.test"],
    );
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
    app.host_ui.borrow_mut().busy = None;
    app.project_host_model(&ui);
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
    app.host_ui.borrow_mut().busy = None;
    app.project_host_model(&ui);
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
    app.host_ui.borrow_mut().busy = None;
    app.project_host_model(&ui);
    state.set_category(1);
    app.reset_host_page(&ui);
    assert_eq!(state.get_companion_mind(), "remote mind");
    state.invoke_save_companion();
    match requests.try_recv().unwrap().command {
        Command::Companion { revision, .. } => assert_eq!(revision, 2),
        _ => panic!("expected refreshed companion save"),
    }
    app.host_ui.borrow_mut().busy = None;
    app.project_host_model(&ui);
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
    app.shutdown(&ui);
    let saved = settings::ClientSettings::load(&temp.path().join("client.json")).unwrap();
    assert_eq!(saved.avatar.config.size_percent, 170);
    assert_eq!(saved.theme_index, 2);
    assert_eq!(saved.font_family, "monospace");
}

fn respond_availability(
    app: &Application,
    ui: &AppWindow,
    requests: &mut tokio::sync::mpsc::Receiver<Request>,
    revision: u64,
    provider: &str,
    destinations: Vec<&str>,
) {
    let mut last = None;
    while let Ok(request) = requests.try_recv() {
        assert!(matches!(request.command, Command::Availability(_)));
        last = request.context;
    }
    let context = last.expect("availability request");
    let value = serde_json::from_value(json!({"companion_id":"c","connection_id":"p","settings_revision":revision,"provider_revision":provider,"destinations":destinations,"connection_fingerprint":{},"operations":[]})).unwrap();
    app.handle_host_event(
        ui,
        Event::Action(Box::new(ActionResultEnvelope {
            context,
            payload: ActionEvent::Availability(value),
        })),
    );
}
