use super::*;

pub(crate) fn bind_tray(
    ui: &ChatWindow,
    settings: &SettingsWindow,
) -> Result<(Rc<tray::Ui>, slint::Timer), Box<dyn Error>> {
    let (tray, receiver) = tray::Ui::new(ui.as_weak());
    let weak = ui.as_weak();
    tray.on_show_chat(move || {
        if let Some(ui) = weak.upgrade() {
            show_chat(&ui);
        }
    });
    let chat_weak = ui.as_weak();
    tray.on_open_settings(move || {
        // Let tray menu activation finish before creating/showing a window.
        // Use the same callback as the chat's Settings button.
        let chat_weak = chat_weak.clone();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = chat_weak.upgrade() {
                ui.invoke_open_settings();
            }
        });
    });
    tray.on_quit(quit);
    settings.on_quit(quit);

    let registration = Rc::new(RefCell::new(None::<tray::Registration>));
    let lease = registration.clone();
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        let Some(ui) = weak.upgrade() else {
            return slint::CloseRequestResponse::KeepWindowShown;
        };
        if lease
            .borrow()
            .as_ref()
            .is_some_and(|s| s.usable_at(Instant::now()))
        {
            slint::CloseRequestResponse::HideWindow
        } else {
            ui.set_tray_status(
                "トレイを利用できないため画面を表示したままにします。設定から終了できます。".into(),
            );
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    let settings_weak = settings.as_weak();
    let monitor_timer = slint::Timer::default();
    monitor_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            for update in receiver.try_iter() {
                *registration.borrow_mut() = Some(update);
            }
            let Some(ui) = weak.upgrade() else { return };
            let ready = registration
                .borrow()
                .as_ref()
                .is_some_and(|s| s.usable_at(Instant::now()));
            ui.set_tray_ready(ready);
            ui.set_tray_status(if ready {
                "× は非表示 · トレイをクリックすると再表示します。".into()
            } else {
                "トレイを利用できません。画面を開いたまま使用できます。".into()
            });
            let settings_visible = if let Some(settings) = settings_weak.upgrade() {
                settings.set_tray_ready(ready);
                settings.set_tray_status(ui.get_tray_status());
                settings.window().is_visible()
            } else {
                false
            };
            if !ready && !ui.window().is_visible() && !settings_visible {
                show_chat(&ui);
            }
        },
    );

    Ok((tray, monitor_timer))
}

pub(super) fn show_chat(ui: &ChatWindow) {
    if let Err(error) = ui.show() {
        eprintln!("会話画面を表示できません: {error}");
    }
    ui.invoke_focus_input();
}

pub(super) fn show_settings(ui: &ChatWindow, settings: &SettingsWindow) {
    settings.set_apply_error("".into());
    settings.set_theme_index(ui.get_theme_index());
    settings.set_avatar_size_percent(ui.get_avatar_size_percent());
    settings.set_avatar_size_text(ui.get_avatar_size_percent().to_string().into());
    settings.set_avatar_failed(ui.get_avatar_failed());
    settings.set_font_family(ui.get_font_family());
    settings.set_text_size(ui.get_text_size());
    settings.set_avatar_model(ui.get_avatar_model());
    settings.set_avatar_motion(ui.get_avatar_motion());
    settings.set_motion_playing(ui.get_motion_playing());
    settings.set_motion_loop(ui.get_motion_loop());
    settings.set_avatar_visible(ui.get_avatar_visible());
    settings.set_avatar_fps_index(ui.get_avatar_fps_index());
    settings.set_tray_ready(ui.get_tray_ready());
    settings.set_tray_status(ui.get_tray_status());
    if let Err(error) = settings.show() {
        eprintln!("設定画面を表示できません: {error}");
    } else {
        settings.window().set_minimized(false);
        settings.window().request_redraw();
    }
}

pub(super) fn quit() {
    if let Err(error) = slint::quit_event_loop() {
        eprintln!("クライアントを終了できません: {error}");
    }
}
