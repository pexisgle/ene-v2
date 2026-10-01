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
    assert_eq!(parse_args([].into_iter()), Ok(Some(Mode::Disconnected)));
    assert_eq!(
        parse_args(["--demo".into()].into_iter()),
        Ok(Some(Mode::Demo))
    );
    assert!(parse_args(["--invalid".into()].into_iter()).is_err());
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
fn enter_and_clipboard_preserve_multiline_japanese_without_sending() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = ChatWindow::new().unwrap();
    let messages = bind_chat(&ui, Mode::Demo);
    ui.show().unwrap();
    ui.invoke_focus_input();
    type_text(&ui, "日本語");
    type_text(&ui, Key::Return);
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
    click(&ui, "送信");
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
    click(&settings, "適用して閉じる");
    assert_eq!(ui.get_font_family(), "Noto Serif CJK JP");
    assert_eq!(ui.get_text_size(), 20);
    assert!(!settings.window().is_visible());
    assert!(ui.get_editor_focused());
}

/// Uses the real Winit Wayland backend and FemtoVG/WGPU renderer. Input events
/// are injected at Slint's window boundary; this does NOT test a native IME.
#[test]
#[ignore = "requires a running Wayland compositor and a Vulkan/GL adapter"]
fn wayland_rendering_and_input() {
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg-wgpu".into())
        .select()
        .unwrap();
    let ui = ChatWindow::new().unwrap();
    let messages = bind_chat(&ui, Mode::Demo);
    let settings = SettingsWindow::new().unwrap();
    bind_settings(&ui, &settings);
    let (_tray, _monitor_timer) = bind_tray(&ui, &settings).unwrap();
    ui.show().unwrap();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    let mut step = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(300),
        move || {
            let ui = weak.upgrade().unwrap();
            match step {
                0 => {
                    save_snapshot(&ui, "desktop-smoke-before.png");
                    ui.invoke_focus_input();
                    type_text(&ui, "日本語の入力確認");
                    type_text(&ui, Key::Return);
                    type_text(&ui, "二行目も送信ボタンまで送られません。");
                    assert_eq!(messages.row_count(), 0);
                    assert!(ui.get_draft().contains('\n'));
                    click(&ui, "送信");
                    assert_eq!(messages.row_count(), 2);
                    assert!(ui.get_draft().is_empty());
                    assert!(ui.get_editor_focused());
                }
                1 => {
                    save_snapshot(&ui, "desktop-smoke-after.png");
                    if !ui.get_tray_ready() {
                        ui.window().dispatch_event(WindowEvent::CloseRequested);
                        assert!(ui.window().is_visible());
                    }
                }
                2 => {
                    assert!(ui.get_conversation_height() > 500.0);
                    ui.invoke_focus_input();
                    type_text(&ui, "長文入力の確認\n".repeat(20));
                }
                3 => {
                    assert!(ui.get_composer_height() > 86.0);
                    assert!(ui.get_composer_height() <= 180.0);
                    save_snapshot(&ui, "desktop-smoke-long-input.png");
                    ui.set_draft("".into());
                    ui.invoke_draft_edited("".into());
                    ui.window().set_size(slint::LogicalSize::new(960.0, 820.0));
                }
                4 => {
                    save_snapshot(&ui, "desktop-smoke-large.png");
                    ui.window().set_size(slint::LogicalSize::new(420.0, 480.0));
                    ui.set_text_size(28);
                }
                5 => {
                    assert!(ui.get_conversation_height() > 200.0);
                    save_snapshot(&ui, "desktop-smoke-compact.png");
                    ui.set_text_size(16);
                    click(&ui, "設定");
                }
                6 => {
                    save_settings_snapshot(&settings);
                    settings.invoke_apply_settings();
                    assert!(!settings.window().is_visible());
                    quit();
                }
                // GPU readback can make a tick overdue before the queued quit
                // reaches Winit. Re-request quit without repeating actions.
                _ => quit(),
            }
            step += 1;
        },
    );
    slint::run_event_loop_until_quit().unwrap();
}

fn save_snapshot(ui: &ChatWindow, name: &str) {
    save_window_snapshot(ui.window(), name);
}

fn save_settings_snapshot(ui: &SettingsWindow) {
    save_window_snapshot(ui.window(), "desktop-smoke-settings.png");
}

fn save_window_snapshot(window: &slint::Window, name: &str) {
    let pixels = window.take_snapshot().unwrap();
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
    std::fs::create_dir_all(&directory).unwrap();
    let file = std::fs::File::create(directory.join(name)).unwrap();
    let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(pixels.as_bytes())
        .unwrap();
    println!("GUI snapshot: {}", directory.join(name).display());
}
