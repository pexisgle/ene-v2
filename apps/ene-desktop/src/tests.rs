use super::*;
use slint::Model;
use slint::platform::{Key, PointerEventButton, WindowEvent};

fn type_text(ui: &ChatWindow, text: impl Into<slint::SharedString>) {
    let text = text.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn control_key(ui: &ChatWindow, key: &str) {
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    type_text(ui, key);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
}

fn click<T: i_slint_backend_testing::ElementRoot + ComponentHandle>(ui: &T, label: &str) {
    let item = i_slint_backend_testing::ElementQuery::from_root(ui)
        .match_descendants()
        .match_predicate({
            let label = label.to_owned();
            move |item| item.accessible_label().is_some_and(|s| s == label)
        })
        .find_first()
        .unwrap_or_else(|| panic!("button missing: {label}"));
    let origin = item.absolute_position();
    let size = item.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2., origin.y + size.height / 2.);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

#[test]
fn demo_is_explicit_and_unknown_arguments_are_rejected() {
    use clap::Parser;
    assert_eq!(
        Args::try_parse_from(["ene-desktop"]).unwrap().mode(),
        Mode::Disconnected
    );
    assert_eq!(
        Args::try_parse_from(["ene-desktop", "--demo"])
            .unwrap()
            .mode(),
        Mode::Demo
    );
    assert!(Args::try_parse_from(["ene-desktop", "--invalid"]).is_err());
    assert_eq!(
        Args::try_parse_from(["ene-desktop", "--help"])
            .unwrap_err()
            .kind(),
        clap::error::ErrorKind::DisplayHelp
    );
}

#[test]
fn whitespace_is_rejected_including_japanese_spaces() {
    for text in ["", " ", "\n\t\r", "\u{3000}\u{00a0}"] {
        assert!(!Mode::Demo.accepts(text));
        assert!(Mode::Demo.respond(text).is_none());
    }
    assert!(Mode::Demo.accepts("日本語\n🙂"));
    assert!(Mode::Disconnected.respond("日本語").is_none());
}

#[test]
fn acceptance_preserves_unicode_clears_draft_and_returns_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let messages = bind_chat(&ui, Mode::Demo);
    ui.invoke_focus_input();
    ui.set_draft("\u{3000}\n ".into());
    ui.invoke_submit();
    assert_eq!(messages.row_count(), 0);
    assert_eq!(ui.get_draft(), "\u{3000}\n ");

    let text = "  日本語の確定\n二行目🙂  ";
    ui.set_draft(text.into());
    ui.invoke_draft_edited(text.into());
    assert!(ui.get_can_send());
    ui.invoke_submit();
    assert_eq!(messages.row_count(), 2);
    assert_eq!(messages.row_data(0).unwrap().body, text);
    assert!(messages.row_data(1).unwrap().provisional);
    assert!(ui.get_draft().is_empty());
    assert!(!ui.get_can_send());
    assert!(ui.get_editor_focused());
    ui.invoke_submit();
    assert_eq!(messages.row_count(), 2);
}

#[test]
fn disconnected_ui_never_produces_a_fake_response() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let messages = bind_chat(&ui, Mode::Disconnected);
    ui.set_draft("こんにちは".into());
    ui.invoke_draft_edited("こんにちは".into());
    ui.invoke_submit();
    assert!(!ui.get_can_send());
    assert_eq!(messages.row_count(), 0);
    assert_eq!(ui.get_draft(), "こんにちは");
}

#[test]
fn shift_enter_and_clipboard_preserve_multiline_and_enter_sends() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let messages = bind_chat(&ui, Mode::Demo);
    ui.show().unwrap();
    ui.invoke_focus_input();
    type_text(&ui, "日本語");
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    type_text(&ui, Key::Return);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Shift.into(),
    });
    type_text(&ui, "二行目🙂");
    assert_eq!(ui.get_draft(), "日本語\n二行目🙂");
    assert_eq!(messages.row_count(), 0);
    control_key(&ui, "a");
    control_key(&ui, "c");
    type_text(&ui, Key::Backspace);
    assert!(ui.get_draft().is_empty());
    control_key(&ui, "v");
    assert_eq!(ui.get_draft(), "日本語\n二行目🙂");
    assert_eq!(messages.row_count(), 0);
    type_text(&ui, Key::Return);
    assert_eq!(messages.row_count(), 2);
    assert!(ui.get_draft().is_empty());
    assert!(ui.get_editor_focused());
}

#[test]
fn settings_apply_to_chat_and_return_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let settings = SettingsWindow::new().unwrap();
    bind_settings(&ui, &settings);
    ui.show().unwrap();
    click(&ui, "設定");
    assert!(settings.window().is_visible());
    assert_eq!(settings.get_font_family(), "Noto Sans CJK JP");
    settings.set_font_family("Noto Serif CJK JP".into());
    settings.set_text_size(20);
    settings.set_theme_index(2);
    click(&settings, "適用して閉じる");
    assert_eq!(ui.get_font_family(), "Noto Serif CJK JP");
    assert_eq!(ui.get_text_size(), 20);
    assert_eq!(ui.get_theme_index(), 2);
    assert!(!settings.window().is_visible());
    assert!(ui.get_editor_focused());
}

#[test]
fn failed_save_preserves_visible_and_persisted_configuration() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let settings = SettingsWindow::new().unwrap();
    let original = avatar::ClientSettings::default();
    reflect_settings(&ui, &settings, None, &original);
    let saved = RefCell::new(original.clone());
    let controller = avatar::Controller::new(|| {});
    let directory = tempfile::tempdir().unwrap();
    let blocker = directory.path().join("保存できない場所");
    std::fs::write(&blocker, b"existing").unwrap();
    let mut updated = original.clone();
    updated.avatar.visible = false;
    assert!(!save_settings(
        updated,
        &saved,
        &ui,
        &settings,
        None,
        &controller,
        &blocker.join("client.json")
    ));
    assert_eq!(saved.borrow().avatar, original.avatar);
    assert!(ui.get_avatar_visible());
    assert!(settings.get_avatar_visible());
    assert!(!settings.get_apply_error().is_empty());
    assert_eq!(std::fs::read(blocker).unwrap(), b"existing");
}
