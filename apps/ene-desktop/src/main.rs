mod avatar;
mod chat;
mod file_picker;
mod tray;

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;
use std::time::{Duration, Instant};

use chat::Mode;
use slint::{ComponentHandle, VecModel};

slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    let mode = match parse_args(std::env::args().skip(1))? {
        Some(mode) => mode,
        None => return Ok(()),
    };
    let ui = ChatWindow::new()?;
    let settings = SettingsWindow::new()?;
    bind_chat(&ui, mode);
    bind_settings(&ui, &settings);
    let file_picker = FilePickerWindow::new()?;
    file_picker::bind(&settings, &file_picker);

    let (tray, monitor_timer) = bind_tray(&ui, &settings)?;
    let (avatar, avatar_timer) = bind_avatar(&ui, &settings, &tray);
    let run_result = ui.show().and_then(|_| {
        ui.invoke_focus_input();
        // Hiding the last window must not stop the client event loop.
        slint::run_event_loop_until_quit()
    });
    // Slint can retain timer callback captures after its loop stops. Explicitly
    // join the supervisor while the UI is still alive, rather than relying on
    // the last Rc being dropped during process teardown.
    avatar.shutdown();
    drop((
        avatar_timer,
        avatar,
        monitor_timer,
        tray,
        file_picker,
        settings,
    ));
    run_result?;
    Ok(())
}

fn bind_tray(
    ui: &ChatWindow,
    settings: &SettingsWindow,
) -> Result<(EneTray, slint::Timer), Box<dyn Error>> {
    let tray = EneTray::new()?;
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
    let receiver = tray::monitor();
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

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Mode>, String> {
    let mut mode = Mode::Disconnected;
    for arg in args {
        match arg.as_str() {
            "--demo" => mode = Mode::Demo,
            "--help" | "-h" => {
                println!(
                    "ene-desktop [--demo]\n  --demo  日本語入力の検証用。固定の仮応答を返します（外部送信なし）。\n  指定なしではホスト未接続画面を表示します。"
                );
                return Ok(None);
            }
            _ => {
                return Err(format!(
                    "未対応の引数: {arg}（--help で起動方法を確認できます）"
                ));
            }
        }
    }
    Ok(Some(mode))
}

fn bind_chat(ui: &ChatWindow, mode: Mode) -> Rc<VecModel<ChatEntry>> {
    ui.set_demo_mode(mode == Mode::Demo);
    ui.set_status(if mode == Mode::Demo {
        "日本語入力を試せます。返答は仮応答です。".into()
    } else {
        "未接続 — ホスト接続は第3段階で実装します。".into()
    });
    let messages = Rc::new(VecModel::<ChatEntry>::default());
    ui.set_messages(messages.clone().into());
    let weak = ui.as_weak();
    ui.on_draft_edited(move |text| {
        if let Some(ui) = weak.upgrade() {
            ui.set_can_send(mode.accepts(&text));
        }
    });
    let model = messages.clone();
    let weak = ui.as_weak();
    ui.on_submit(move || {
        let Some(ui) = weak.upgrade() else { return };
        let draft = ui.get_draft();
        let Some(response) = mode.respond(&draft) else {
            return;
        };
        model.push(ChatEntry {
            author: "あなた".into(),
            body: draft,
            provisional: false,
        });
        model.push(ChatEntry {
            author: "ene".into(),
            body: response.into(),
            provisional: true,
        });
        ui.set_draft("".into());
        ui.set_can_send(false);
        ui.set_status("仮応答を表示しました。".into());
        ui.invoke_focus_input();
        // Layout is deferred; scrolling now would use the old content height.
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_scroll_to_latest();
            }
        });
    });
    messages
}

fn apply_theme(ui: &ChatWindow, settings: &SettingsWindow, index: i32) {
    ui.global::<Theme>().invoke_apply(index);
    settings.global::<Theme>().invoke_apply(index);
    ui.set_theme_index(index);
    settings.set_theme_index(index);
}

fn bind_settings(ui: &ChatWindow, settings: &SettingsWindow) {
    // Prepare the hidden window at startup, including its native adapter,
    // instead of relying on an earlier visit from the chat window.
    settings
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    let weak = settings.as_weak();
    let chat_weak = ui.as_weak();
    ui.on_open_settings(move || {
        if let (Some(settings), Some(ui)) = (weak.upgrade(), chat_weak.upgrade()) {
            show_settings(&ui, &settings);
        }
    });
    let weak = settings.as_weak();
    let chat_weak = ui.as_weak();
    settings.on_apply_settings(move || {
        if let (Some(settings), Some(ui)) = (weak.upgrade(), chat_weak.upgrade()) {
            if !settings.invoke_commit_avatar_settings() {
                return;
            }
            apply_theme(&ui, &settings, settings.get_theme_index());
            let font = settings.get_font_family();
            if !font.trim().is_empty() {
                ui.set_font_family(font);
            }
            ui.set_text_size(settings.get_text_size().clamp(12, 28));
            show_chat(&ui);
            let _ = settings.hide();
        }
    });
}

fn show_chat(ui: &ChatWindow) {
    if let Err(error) = ui.show() {
        eprintln!("会話画面を表示できません: {error}");
    }
    ui.invoke_focus_input();
}

fn show_settings(ui: &ChatWindow, settings: &SettingsWindow) {
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

fn quit() {
    if let Err(error) = slint::quit_event_loop() {
        eprintln!("クライアントを終了できません: {error}");
    }
}

#[cfg(test)]
mod tests;

fn bind_avatar(
    ui: &ChatWindow,
    settings: &SettingsWindow,
    tray: &EneTray,
) -> (Rc<avatar::Controller>, slint::Timer) {
    use ene_shared::avatar::AvatarEvent;
    let path = avatar::config_path();
    let initial = avatar::ClientSettings::load(&path).unwrap_or_else(|error| {
        eprintln!("{error}");
        avatar::ClientSettings::default()
    });
    apply_theme(ui, settings, initial.theme_index);
    ui.set_avatar_size_percent(initial.avatar.size_percent as i32);
    settings.set_avatar_size_percent(initial.avatar.size_percent as i32);
    settings.set_avatar_size_text(initial.avatar.size_percent.to_string().into());
    ui.set_font_family(initial.font_family.clone().into());
    ui.set_text_size(initial.text_size);
    ui.set_avatar_model(initial.avatar.model.clone().into());
    ui.set_avatar_motion(initial.avatar.motion.clone().unwrap_or_default().into());
    ui.set_motion_playing(initial.avatar.motion_playing);
    ui.set_motion_loop(initial.avatar.motion_loop);
    ui.set_avatar_visible(initial.avatar.visible);
    ui.set_avatar_fps_index(i32::from(initial.avatar.fps == 60));
    settings.set_avatar_model(initial.avatar.model.clone().into());
    settings.set_avatar_motion(ui.get_avatar_motion());
    settings.set_motion_playing(ui.get_motion_playing());
    settings.set_motion_loop(ui.get_motion_loop());
    settings.set_avatar_visible(initial.avatar.visible);
    settings.set_avatar_fps_index(i32::from(initial.avatar.fps == 60));
    ui.set_avatar_status("キャラクターを読み込んでいます…".into());
    settings.set_avatar_status(ui.get_avatar_status());
    tray.set_avatar_visible(initial.avatar.visible);
    let saved = Rc::new(RefCell::new(initial));
    let signal = ui.as_weak();
    let controller = Rc::new(avatar::Controller::new(move || {
        let _ = signal.upgrade_in_event_loop(|ui| ui.invoke_avatar_events_available());
    }));
    controller.configure(saved.borrow().avatar.clone());
    let c = controller.clone();
    let config = saved.clone();
    ui.on_restart_avatar(move || c.restart(config.borrow().avatar.clone()));
    let c = controller.clone();
    let config = saved.clone();
    settings.on_restart_avatar(move || c.restart(config.borrow().avatar.clone()));
    let c = controller.clone();
    let config = saved.clone();
    tray.on_restart_avatar(move || c.restart(config.borrow().avatar.clone()));
    let c = controller.clone();
    let config = saved.clone();
    let sw = settings.as_weak();
    let uw = ui.as_weak();
    let tw = tray.as_weak();
    settings.on_recenter_avatar(move || {
        let mut config = config.borrow_mut();
        config.avatar.visible = true;
        if let Some(ui) = uw.upgrade() {
            ui.set_avatar_visible(true);
            if let Err(error) = config.save(&avatar::config_path()) {
                ui.set_avatar_failed(true);
                ui.set_avatar_status(error.into());
            }
        }
        if let Some(settings) = sw.upgrade() {
            settings.set_avatar_visible(true);
        }
        if let Some(tray) = tw.upgrade() {
            tray.set_avatar_visible(true);
        }
        c.configure(config.avatar.clone());
        c.recenter();
    });
    let sw = settings.as_weak();
    tray.on_recenter_avatar(move || {
        if let Some(settings) = sw.upgrade() {
            settings.invoke_recenter_avatar();
        }
    });
    let c = controller.clone();
    let config = saved.clone();
    let sw = settings.as_weak();
    let uw = ui.as_weak();
    let tw = tray.as_weak();
    tray.on_toggle_avatar(move || {
        let mut config = config.borrow_mut();
        config.avatar.visible = !config.avatar.visible;
        if let Some(ui) = uw.upgrade() {
            ui.set_avatar_visible(config.avatar.visible);
        }
        if let Err(error) = config.save(&avatar::config_path())
            && let Some(ui) = uw.upgrade()
        {
            ui.set_avatar_failed(true);
            ui.set_avatar_status(error.into());
        }
        if let Some(settings) = sw.upgrade() {
            settings.set_avatar_visible(config.avatar.visible);
        }
        if let Some(tray) = tw.upgrade() {
            tray.set_avatar_visible(config.avatar.visible);
        }
        c.configure(config.avatar.clone());
    });
    let c = controller.clone();
    let config = saved.clone();
    let sw = settings.as_weak();
    let uw = ui.as_weak();
    let tw = tray.as_weak();
    settings.on_commit_avatar_settings(move || {
        if let (Some(settings), Some(ui)) = (sw.upgrade(), uw.upgrade()) {
            settings.set_apply_error("".into());
            let mut updated = config.borrow().clone();
            let font = settings.get_font_family();
            if !font.trim().is_empty() {
                updated.font_family = font.to_string();
            }
            updated.theme_index = settings.get_theme_index().clamp(0, 2);
            let size = settings.get_avatar_size_text();
            let Ok(size) = size.trim().parse::<u32>() else {
                settings.set_apply_error(
                    "キャラクターの大きさは50〜200の整数で入力してください。".into(),
                );
                return false;
            };
            if !(50..=200).contains(&size) {
                settings.set_apply_error("キャラクターの大きさは50〜200%を選んでください。".into());
                return false;
            }
            updated.avatar.size_percent = size;
            updated.text_size = settings.get_text_size().clamp(12, 28);
            updated.avatar.model = settings.get_avatar_model().to_string();
            let motion = settings.get_avatar_motion();
            updated.avatar.motion = if motion.is_empty() {
                None
            } else {
                Some(motion.to_string())
            };
            updated.avatar.motion_playing = settings.get_motion_playing();
            updated.avatar.motion_loop = settings.get_motion_loop();
            updated.avatar.visible = settings.get_avatar_visible();
            updated.avatar.fps = if settings.get_avatar_fps_index() == 1 {
                60
            } else {
                30
            };
            if let Err(error) = updated.save(&avatar::config_path()) {
                settings.set_apply_error(error.into());
                return false;
            }
            ui.set_avatar_size_percent(updated.avatar.size_percent as i32);
            settings.set_avatar_size_percent(updated.avatar.size_percent as i32);
            settings.set_avatar_size_text(updated.avatar.size_percent.to_string().into());
            ui.set_avatar_model(updated.avatar.model.clone().into());
            ui.set_avatar_motion(updated.avatar.motion.clone().unwrap_or_default().into());
            ui.set_motion_playing(updated.avatar.motion_playing);
            ui.set_motion_loop(updated.avatar.motion_loop);
            ui.set_avatar_visible(updated.avatar.visible);
            ui.set_avatar_fps_index(i32::from(updated.avatar.fps == 60));
            if let Some(tray) = tw.upgrade() {
                tray.set_avatar_visible(updated.avatar.visible);
            }
            c.configure(updated.avatar.clone());
            *config.borrow_mut() = updated;
            return true;
        }
        false
    });
    let weak = ui.as_weak();
    let sw = settings.as_weak();
    let tw = tray.as_weak();
    let c = controller.clone();
    let timer = slint::Timer::default();
    ui.on_avatar_events_available(move || {
        let Some(ui) = weak.upgrade() else { return };
        for event in c.events.try_iter() {
            match event {
                AvatarEvent::MotionError { message } => {
                    ui.set_motion_failed(true);
                    ui.set_motion_status(message.into());
                }
                AvatarEvent::MotionState { state } => {
                    use ene_shared::avatar::MotionState;
                    let status = match state {
                        MotionState::None if saved.borrow().avatar.motion.is_some() => None,
                        MotionState::None => Some("静止姿勢です。"),
                        MotionState::Loading => Some("モーションを読み込んでいます…"),
                        MotionState::Playing => Some("モーションを再生しています。"),
                        MotionState::Paused => Some("モーションは一時停止中です。"),
                        MotionState::Finished => Some("モーションの再生が完了しました。"),
                    };
                    if let Some(status) = status {
                        ui.set_motion_failed(false);
                        ui.set_motion_status(status.into());
                    }
                }
                AvatarEvent::Ready { model } => {
                    ui.set_avatar_failed(false);
                    ui.set_avatar_status(format!("表示中 · {model}").into());
                }
                AvatarEvent::Error { message } => {
                    ui.set_avatar_failed(true);
                    ui.set_avatar_status(message.into());
                }
                AvatarEvent::ShowChat => show_chat(&ui),
                AvatarEvent::PositionChanged {
                    right,
                    bottom,
                    output,
                } => {
                    let mut config = saved.borrow_mut();
                    config.avatar.right = right;
                    config.avatar.bottom = bottom;
                    config.avatar.output = output;
                    if let Err(error) = config.save(&avatar::config_path()) {
                        ui.set_avatar_failed(true);
                        ui.set_avatar_status(error.into());
                    }
                }
                AvatarEvent::VisibilityChanged { visible } => {
                    if !visible {
                        ui.set_avatar_status("キャラクターは非表示です。".into());
                    }
                    if let Some(tray) = tw.upgrade() {
                        tray.set_avatar_visible(visible);
                    }
                }
                AvatarEvent::Stopped => {}
            }
        }
        if let Some(settings) = sw.upgrade() {
            settings.set_avatar_status(ui.get_avatar_status());
            settings.set_avatar_failed(ui.get_avatar_failed());
            settings.set_motion_status(ui.get_motion_status());
        }
        if let Some(tray) = tw.upgrade() {
            tray.set_avatar_failed(ui.get_avatar_failed());
        }
    });
    // A fallback tick also drains startup events queued before the loop starts.
    let signal = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            if let Some(ui) = signal.upgrade() {
                ui.invoke_avatar_events_available();
            }
        },
    );
    (controller, timer)
}
