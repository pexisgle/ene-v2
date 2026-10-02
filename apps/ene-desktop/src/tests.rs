use super::*;
use slint::Model;
use std::rc::Rc;

fn startup_fixture(mode: Mode) -> (AppWindow, Rc<app::Application>, tempfile::TempDir) {
    let ui = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let app = app::Application::bind(
        &ui,
        mode,
        directory.path().join("client.json"),
        settings::ClientSettings::default(),
        None,
        None,
    );
    (ui, app, directory)
}

fn fixture(mode: Mode) -> (AppWindow, Rc<app::Application>, tempfile::TempDir) {
    let result = startup_fixture(mode);
    if mode == Mode::Demo {
        result.0.global::<UiState>().invoke_start_demo();
    }
    result
}

use slint::platform::{Key, PointerEventButton, WindowEvent};

fn type_text(ui: &AppWindow, text: impl Into<slint::SharedString>) {
    let text = text.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn control_key(ui: &AppWindow, key: &str) {
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
fn settings_save_in_place_and_navigation_preserves_drafts() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, directory) = fixture(Mode::Demo);
    ui.show().unwrap();
    ui.global::<UiState>().set_draft("途中の文章".into());
    click(&ui, "設定");
    assert_eq!(ui.global::<UiState>().get_page(), 1);
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.font_family = "Noto Serif CJK JP".into();
    draft.text_size = 20;
    draft.theme_index = 2;
    ui.global::<UiState>().set_settings_draft(draft.clone());
    ui.global::<UiState>().invoke_settings_edited();
    assert!(ui.global::<UiState>().get_settings_dirty());
    click(&ui, "対話に戻る");
    assert_eq!(ui.global::<UiState>().get_draft(), "途中の文章");
    assert!(ui.get_editor_focused());
    click(&ui, "設定");
    assert_eq!(ui.global::<UiState>().get_settings_draft(), draft);
    click(&ui, "保存");
    assert_eq!(ui.global::<UiState>().get_page(), 1);
    assert!(ui.window().is_visible());
    assert_eq!(ui.global::<UiState>().get_text_size(), 20);
    assert!(!ui.global::<UiState>().get_settings_dirty());
    assert_eq!(
        settings::ClientSettings::load(&directory.path().join("client.json"))
            .unwrap()
            .text_size,
        20
    );
    assert_eq!(app.settings.borrow().saved.theme_index, 2);
}

#[test]
fn invalid_and_failed_save_preserve_configuration_and_allow_retry() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, directory) = fixture(Mode::Demo);
    app::navigate(&ui, 1);
    let original = app.settings.borrow().saved.clone();
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.size_text = "999".into();
    ui.global::<UiState>().set_settings_draft(draft);
    ui.global::<UiState>().invoke_save_settings();
    assert_eq!(app.settings.borrow().saved, original);
    assert_eq!(ui.global::<UiState>().get_error_field(), "size");
    assert_eq!(ui.global::<UiState>().get_settings_draft().size_text, "999");
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.size_text = "125".into();
    ui.global::<UiState>().set_settings_draft(draft);
    let path = directory.path().join("client.json");
    std::fs::create_dir(&path).unwrap();
    ui.global::<UiState>().invoke_save_settings();
    assert_eq!(app.settings.borrow().saved, original);
    assert!(!ui.global::<UiState>().get_apply_error().is_empty());
    assert!(ui.global::<UiState>().get_settings_dirty());
    std::fs::remove_dir(path).unwrap();
    ui.global::<UiState>().invoke_save_settings();
    assert_eq!(app.settings.borrow().saved.avatar.size_percent, 125);
    assert!(ui.global::<UiState>().get_apply_error().is_empty());
}

#[test]
fn live_position_and_visibility_changes_do_not_overwrite_edits() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, _) = fixture(Mode::Demo);
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.size_text = "150".into();
    ui.global::<UiState>().set_settings_draft(draft);
    ui.global::<UiState>().invoke_settings_edited();
    app.handle_avatar_event(
        &ui,
        ene_protocol::avatar::AvatarEvent::PositionChanged {
            right: -250,
            bottom: 180,
            output: Some("DP-2".into()),
        },
    );
    app.toggle_avatar(&ui);
    assert_eq!(ui.global::<UiState>().get_settings_draft().size_text, "150");
    ui.global::<UiState>().invoke_save_settings();
    let value = app.settings.borrow().saved.clone();
    assert_eq!(value.avatar.size_percent, 150);
    assert_eq!(value.avatar.right, -250);
    assert_eq!(value.avatar.bottom, 180);
    assert_eq!(value.avatar.output.as_deref(), Some("DP-2"));
    assert!(!value.avatar.visible);
}

#[test]
fn reset_and_save_invalidate_late_file_selections() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, _) = fixture(Mode::Demo);
    let before = app.settings.borrow().generation;
    ui.global::<UiState>().invoke_reset_settings();
    app.selection_result(&ui, before, false, Ok(Some("/tmp/古い選択.vrm".into())));
    assert!(
        !ui.global::<UiState>()
            .get_settings_draft()
            .model
            .contains("古い選択")
    );
    let current = app.settings.borrow().generation;
    app.selection_result(&ui, current, true, Ok(Some("/tmp/モーション.vrma".into())));
    assert_eq!(
        ui.global::<UiState>().get_settings_draft().motion,
        "/tmp/モーション.vrma"
    );
    assert!(ui.global::<UiState>().get_settings_dirty());
    ui.global::<UiState>().invoke_save_settings();
    app.selection_result(&ui, current, true, Err("古いエラー".into()));
    assert!(ui.global::<UiState>().get_apply_error().is_empty());
}

#[test]
fn quitting_with_unsaved_changes_offers_return_without_losing_them() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _app, _) = fixture(Mode::Demo);
    app::navigate(&ui, 1);
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.size_text = "160".into();
    ui.global::<UiState>().set_settings_draft(draft);
    ui.global::<UiState>().invoke_settings_edited();
    ui.global::<UiState>().invoke_request_quit();
    assert!(ui.global::<UiState>().get_quit_confirmation());
    assert!(ui.window().is_visible());
    click(&ui, "戻る");
    assert!(!ui.global::<UiState>().get_quit_confirmation());
    assert_eq!(ui.global::<UiState>().get_settings_draft().size_text, "160");
}

#[test]
fn resizing_preserves_editor_and_large_text_keeps_actions_on_screen() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _app, _) = fixture(Mode::Demo);
    ui.global::<UiState>().set_tray_ready(true);
    ui.global::<UiState>().set_avatar_ready(true);
    ui.show().unwrap();
    ui.invoke_focus_input();
    type_text(&ui, "編集中の日本語");
    for size in [12, 16, 28] {
        ui.global::<UiState>().set_text_size(size);
        let scale = (size as f32 / 16.).max(1.);
        let category_boundary = 640. * scale + 24.;
        let form_boundary = 720. * scale + 60.;
        for (width, height) in [
            (420., 480.),
            (720., 760.),
            (1280., 900.),
            (663., 720.),
            (665., 720.),
            (category_boundary - 1., 760.),
            (category_boundary + 1., 760.),
            (form_boundary - 1., 760.),
            (form_boundary + 1., 760.),
        ] {
            ui.window().set_size(slint::LogicalSize::new(width, height));
            i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
            // Querying geometry forces the deferred layout.
            assert!(ui.get_conversation_height() > 0.);
            assert!(ui.get_composer_height() > 0.);
            assert_eq!(ui.global::<UiState>().get_draft(), "編集中の日本語");
            assert!(ui.get_editor_focused());
            app::navigate(&ui, 1);
            let padding = if width < 680. * scale { 12. } else { 20. };
            assert_eq!(ui.get_compact(), width - padding * 2. < 640. * scale);
            assert_eq!(
                ui.get_settings_form_wide(),
                ui.get_settings_form_width() >= 560. * scale
            );
            for label in ["保存", "変更を戻す"] {
                let item = i_slint_backend_testing::ElementQuery::from_root(&ui)
                    .match_predicate(move |item| {
                        item.accessible_label().is_some_and(|v| v == label)
                    })
                    .find_first()
                    .unwrap();
                let pos = item.absolute_position();
                let item_size = item.size();
                assert!(pos.x >= 0. && pos.y >= 0., "{label}: {pos:?}");
                assert!(
                    pos.x + item_size.width <= width + 1.,
                    "{label} spills at {width} with font {size}"
                );
                assert!(
                    pos.y + item_size.height <= height + 1.,
                    "{label} spills below {height} with font {size}"
                );
            }
            app::navigate(&ui, 0);
        }
    }
}

/// Uses installed fonts instead of mocked metrics.
#[test]
fn real_font_layout_reflows_and_preserves_input() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            ..Default::default()
        },
    )))
    .unwrap();
    let (ui, _app, _) = startup_fixture(Mode::Demo);
    ui.global::<UiState>().set_tray_ready(true);
    ui.global::<UiState>().set_avatar_ready(true);
    ui.show().unwrap();
    for (name, width, height, font, page) in [
        ("host-selection", 720., 760., 16, 2),
        ("conversation", 720., 760., 16, 0),
        ("settings", 720., 760., 16, 1),
        ("settings-wide", 1280., 900., 16, 1),
        ("settings-small", 420., 480., 16, 1),
        ("conversation-large-text", 420., 480., 28, 0),
        ("settings-large-text", 420., 480., 28, 1),
        ("host-selection-large-text", 420., 480., 28, 2),
    ] {
        ui.window().set_size(slint::LogicalSize::new(width, height));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        ui.global::<UiState>().set_text_size(font);
        if page == 2 {
            app::show_host_selection(&ui);
        } else {
            ui.global::<UiState>().invoke_start_demo();
            app::navigate(&ui, page);
        }
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));
        let snapshot = ui.window().take_snapshot().unwrap();
        if page != 2 {
            let navigation_label = if page == 0 {
                "設定"
            } else {
                "対話に戻る"
            };
            let navigation = i_slint_backend_testing::ElementQuery::from_root(&ui)
                .match_predicate(move |item| {
                    item.accessible_label()
                        .is_some_and(|v| v == navigation_label)
                })
                .find_first()
                .unwrap();
            assert!(
                navigation.size().width <= 112. * (font as f32 / 16.).max(1.),
                "navigation should stay secondary: {name}"
            );
        }
        assert_eq!(snapshot.width(), width as u32);
        assert_eq!(snapshot.height(), height as u32);
        if page == 0 {
            assert!(ui.get_conversation_height() >= 40.);
        } else {
            let labels = if page == 2 {
                vec![
                    "この端末の設定",
                    "クライアント終了",
                    if ui.global::<UiState>().get_session_active() {
                        "対話を続ける"
                    } else {
                        "対話を始める"
                    },
                ]
            } else {
                vec!["保存", "変更を戻す"]
            };
            for label in labels {
                let item = i_slint_backend_testing::ElementQuery::from_root(&ui)
                    .match_predicate(move |item| {
                        item.accessible_label().is_some_and(|v| v == label)
                    })
                    .find_first()
                    .unwrap();
                let pos = item.absolute_position();
                let size = item.size();
                assert!(
                    pos.x + size.width <= width + 1.,
                    "{name}: {label} horizontal overflow"
                );
                assert!(
                    pos.y + size.height <= height + 1.,
                    "{name}: {label} vertical overflow"
                );
            }
        }
    }
}

#[test]
fn keyboard_focus_reveals_settings_and_resize_preserves_selection() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, _app, _) = fixture(Mode::Demo);
    ui.show().unwrap();
    ui.window().set_size(slint::LogicalSize::new(420., 480.));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    app::navigate(&ui, 1);
    for _ in 0..32 {
        type_text(&ui, Key::Tab);
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        if ui.get_settings_scroll() < -100. {
            break;
        }
    }
    assert!(ui.get_settings_scroll() < -100.);
    let position = ui.get_settings_scroll();
    app::navigate(&ui, 0);
    type_text(&ui, "日本語の選択範囲🙂");
    control_key(&ui, "a");
    ui.window().set_size(slint::LogicalSize::new(1280., 900.));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    control_key(&ui, "c");
    type_text(&ui, Key::Backspace);
    control_key(&ui, "v");
    assert_eq!(ui.global::<UiState>().get_draft(), "日本語の選択範囲🙂");
    ui.window().set_size(slint::LogicalSize::new(420., 480.));
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    app::navigate(&ui, 1);
    assert_eq!(ui.get_settings_scroll(), position);
}

#[test]
fn incoming_messages_do_not_move_history_while_reading() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, _) = fixture(Mode::Demo);
    ui.show().unwrap();
    for n in 0..40 {
        app.messages.push(ChatEntry {
            author: "ene".into(),
            body: format!("長い日本語の発言記録 {n}。\n二行目を表示します。").into(),
            provisional: true,
        });
    }
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    ui.invoke_scroll_to_latest();
    let bottom = ui.get_history_scroll();
    i_slint_backend_testing::ElementQuery::from_root(&ui)
        .match_id("ConversationView::history")
        .find_first()
        .unwrap()
        .scroll(0., 200.);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    let reading = ui.get_history_scroll();
    assert!(reading > bottom);
    app.messages.push(ChatEntry {
        author: "ene".into(),
        body: "新しい発言が届いても閲覧位置を動かしません。".into(),
        provisional: true,
    });
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(ui.get_history_scroll(), reading);
    app::navigate(&ui, 1);
    app::navigate(&ui, 0);
    assert_eq!(ui.get_history_scroll(), reading);
    ui.global::<UiState>().set_draft("送信したら末尾へ".into());
    ui.global::<UiState>().invoke_submit();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert!(ui.get_history_scroll() < reading);
}

/// Requires a running KDE/Wayland session; never edits the user's configuration.
#[test]
#[ignore = "requires a Linux desktop, tray and built ene-avatar"]
fn native_wayland_window_tray_and_avatar_smoke() {
    let ui = AppWindow::new().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let (tray, registered) = tray::Ui::new(ui.as_weak());
    let signal = ui.as_weak();
    let avatar = Rc::new(avatar::Controller::new(move || {
        let _ = signal.upgrade_in_event_loop(|ui| ui.invoke_avatar_events_available());
    }));
    let app = app::Application::bind(
        &ui,
        Mode::Demo,
        directory.path().join("client.json"),
        settings::ClientSettings::default(),
        Some(avatar.clone()),
        Some(tray),
    );
    let (registration_sender, registration_receiver) = std::sync::mpsc::channel();
    let timers = app.start_services(&ui, registration_receiver);
    file_picker::bind(&ui, &app);
    ui.show().unwrap();
    ui.invoke_focus_entry();
    let driver = slint::Timer::default();
    let view = ui.as_weak();
    let application = Rc::downgrade(&app);
    let stage = std::cell::Cell::new(0);
    driver.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(500),
        move || {
            let ui = view.upgrade().unwrap();
            let app = application.upgrade().unwrap();
            let n = stage.get();
            if n != 3 && n != 4 {
                for update in registered.try_iter() {
                    registration_sender.send(update).unwrap();
                }
            }
            match n {
                0 => {
                    assert!(ui.global::<UiState>().get_host_selection_visible());
                    type_text(&ui, Key::Return);
                    assert!(!ui.global::<UiState>().get_host_selection_visible());
                    assert_eq!(ui.global::<UiState>().get_page(), 0);
                    ui.window().set_size(slint::LogicalSize::new(420., 480.));
                    ui.global::<UiState>().set_text_size(28);
                    app::navigate(&ui, 1);
                }
                1 => {
                    assert!(ui.window().take_snapshot().is_ok());
                    app::navigate(&ui, 0);
                    type_text(&ui, "日本語の途中入力");
                    ui.window().set_size(slint::LogicalSize::new(720., 760.));
                }
                2 => assert_eq!(ui.global::<UiState>().get_draft(), "日本語の途中入力"),
                3 => {
                    ui.hide().unwrap();
                    registration_sender
                        .send(tray::Registration {
                            checked_at: std::time::Instant::now(),
                            registered: false,
                        })
                        .unwrap();
                }
                4 => {
                    assert!(
                        ui.window().is_visible(),
                        "tray loss must restore the window"
                    );
                    ui.window().dispatch_event(WindowEvent::CloseRequested);
                    assert!(
                        ui.window().is_visible(),
                        "closing without a tray must be refused"
                    );
                }
                5 => {
                    app.toggle_avatar(&ui);
                    assert!(!ui.global::<UiState>().get_avatar_visible());
                }
                6 => {
                    app.toggle_avatar(&ui);
                    ui.invoke_tray_action(1);
                }
                7 => {
                    assert_eq!(ui.global::<UiState>().get_page(), 1);
                    let mut draft = ui.global::<UiState>().get_settings_draft();
                    draft.size_text = "125".into();
                    ui.global::<UiState>().set_settings_draft(draft);
                    ui.global::<UiState>().invoke_settings_edited();
                    ui.invoke_tray_action(5);
                    assert!(ui.global::<UiState>().get_host_selection_visible());
                    app.request_quit(&ui);
                    assert!(ui.global::<UiState>().get_quit_confirmation());
                    type_text(&ui, Key::Return);
                    assert!(!ui.global::<UiState>().get_quit_confirmation());
                    assert!(ui.global::<UiState>().get_host_selection_visible());
                }
                8 => {
                    eprintln!(
                        "native tray: {}; avatar: {}",
                        ui.global::<UiState>().get_tray_ready(),
                        ui.global::<UiState>().get_avatar_status()
                    );
                    assert!(
                        ui.global::<UiState>().get_avatar_ready(),
                        "renderer never became ready"
                    );
                    assert!(!ui.global::<UiState>().get_avatar_failed());
                    assert!(!ui.global::<UiState>().get_motion_failed());
                    if ui.global::<UiState>().get_tray_ready() {
                        ui.window().dispatch_event(WindowEvent::CloseRequested);
                        assert!(!ui.window().is_visible());
                    }
                    ui.invoke_tray_action(0);
                    assert_eq!(ui.global::<UiState>().get_page(), 0);
                    assert!(ui.window().is_visible());
                    slint::quit_event_loop().unwrap();
                }
                _ => unreachable!(),
            }
            stage.set(n + 1);
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    avatar.shutdown();
    drop((driver, timers, app, avatar));
}

#[test]
fn dpi_and_long_content_keep_fixed_actions_reachable() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("software".into()),
            mock_time: true,
            ..Default::default()
        },
    )))
    .unwrap();
    let (ui, _app, _) = fixture(Mode::Demo);
    ui.show().unwrap();
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.model = format!(
        "/tmp/{}/{}.vrm",
        "非常に長い日本語フォルダ名".repeat(12),
        "model".repeat(80)
    )
    .into();
    ui.global::<UiState>().set_settings_draft(draft);
    ui.global::<UiState>().invoke_settings_edited();
    ui.global::<UiState>().set_apply_error(
        "保存できません。長いエラーの詳細は本文内で確認できます。"
            .repeat(12)
            .into(),
    );
    app::navigate(&ui, 1);
    for (font, scale_factor) in [(16, 1.), (28, 1.5), (28, 2.)] {
        ui.global::<UiState>().set_text_size(font);
        ui.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
        // TestingBackend's adapter assumes scale 1 when setting size.
        // Supply its physical buffer size, then the logical resize event.
        ui.window().set_size(slint::PhysicalSize::new(
            (420. * scale_factor) as u32,
            (480. * scale_factor) as u32,
        ));
        ui.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(420., 480.),
        });
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(200));
        for label in ["保存", "変更を戻す", "対話に戻る", "エラーの詳細"] {
            let item = i_slint_backend_testing::ElementQuery::from_root(&ui)
                .match_predicate(move |item| item.accessible_label().is_some_and(|v| v == label))
                .find_first()
                .unwrap();
            let pos = item.absolute_position();
            let size = item.size();
            assert!(pos.x >= 0. && pos.y >= 0.);
            assert!(
                pos.x + size.width <= 421.,
                "{label}: horizontal overflow, font {font}, DPI {scale_factor}"
            );
            assert!(
                pos.y + size.height <= 481.,
                "{label}: vertical overflow, font {font}, DPI {scale_factor}"
            );
        }
    }
}

#[test]
fn startup_and_return_to_host_selection_preserve_session_and_unsaved_settings() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, _) = startup_fixture(Mode::Demo);
    ui.show().unwrap();
    ui.invoke_focus_entry();
    assert!(ui.global::<UiState>().get_host_selection_visible());
    assert!(!ui.global::<UiState>().get_session_active());
    ui.global::<UiState>()
        .set_draft("選択前には送信しない".into());
    ui.global::<UiState>().invoke_submit();
    assert_eq!(app.messages.row_count(), 0);
    click(&ui, "対話を始める");
    assert!(ui.global::<UiState>().get_session_active());
    assert!(!ui.global::<UiState>().get_host_selection_visible());
    ui.global::<UiState>().invoke_submit();
    assert_eq!(app.messages.row_count(), 2);
    ui.global::<UiState>().set_draft("会話の下書き".into());
    click(&ui, "設定");
    let mut draft = ui.global::<UiState>().get_settings_draft();
    draft.size_text = "135".into();
    ui.global::<UiState>().set_settings_draft(draft.clone());
    ui.global::<UiState>().invoke_settings_edited();
    click(&ui, "ホスト選択");
    assert!(ui.global::<UiState>().get_host_selection_visible());
    click(&ui, "クライアント終了");
    assert!(ui.global::<UiState>().get_quit_confirmation());
    type_text(&ui, Key::Return);
    assert!(!ui.global::<UiState>().get_quit_confirmation());
    assert!(ui.global::<UiState>().get_host_selection_visible());
    assert_eq!(ui.global::<UiState>().get_settings_draft(), draft);
    click(&ui, "対話を続ける");
    assert_eq!(ui.global::<UiState>().get_page(), 0);
    assert_eq!(ui.global::<UiState>().get_draft(), "会話の下書き");
    assert_eq!(app.messages.row_count(), 2);
    click(&ui, "設定");
    assert_eq!(ui.global::<UiState>().get_settings_draft(), draft);
    assert!(ui.global::<UiState>().get_settings_dirty());
}

#[test]
fn disconnected_startup_offers_settings_without_claiming_a_host_session() {
    i_slint_backend_testing::init_no_event_loop();
    let (ui, app, _) = startup_fixture(Mode::Disconnected);
    ui.show().unwrap();
    assert!(ui.global::<UiState>().get_host_selection_visible());
    ui.global::<UiState>().invoke_start_demo();
    assert!(!ui.global::<UiState>().get_session_active());
    ui.global::<UiState>().set_category(3);
    click(&ui, "この端末の設定");
    assert_eq!(ui.global::<UiState>().get_category(), 0);
    assert!(!ui.global::<UiState>().get_host_selection_visible());
    assert_eq!(ui.global::<UiState>().get_page(), 1);
    click(&ui, "ホスト選択に戻る");
    assert!(ui.global::<UiState>().get_host_selection_visible());
    app.handle_avatar_event(&ui, ene_protocol::avatar::AvatarEvent::ShowChat);
    assert!(ui.global::<UiState>().get_host_selection_visible());
    assert!(!ui.global::<UiState>().get_session_active());
    assert_eq!(app.messages.row_count(), 0);
}
