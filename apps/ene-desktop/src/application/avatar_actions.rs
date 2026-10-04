use super::*;

pub(crate) fn bind_avatar(
    ui: &ChatWindow,
    settings: &SettingsWindow,
    tray: &Rc<tray::Ui>,
) -> (Rc<avatar_service::Controller>, slint::Timer) {
    use ene_protocol::avatar::AvatarEvent;
    let path = avatar_service::config_path();
    let initial = avatar_service::ClientSettings::load(&path).unwrap_or_else(|error| {
        eprintln!("{error}");
        avatar_service::ClientSettings::default()
    });
    reflect_settings(ui, settings, Some(tray), &initial);
    ui.set_avatar_status("キャラクターを読み込んでいます…".into());
    settings.set_avatar_status(ui.get_avatar_status());
    let saved = Rc::new(RefCell::new(initial));
    let signal = ui.as_weak();
    let controller = Rc::new(avatar_service::Controller::new(move || {
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
        if let (Some(ui), Some(settings)) = (uw.upgrade(), sw.upgrade()) {
            let mut updated = config.borrow().clone();
            updated.avatar.visible = true;
            if save_settings(
                updated,
                &config,
                &ui,
                &settings,
                tw.upgrade().as_deref(),
                &c,
                &avatar_service::config_path(),
            ) {
                c.recenter();
            }
        }
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
        if let (Some(ui), Some(settings)) = (uw.upgrade(), sw.upgrade()) {
            let mut updated = config.borrow().clone();
            updated.avatar.visible = !updated.avatar.visible;
            save_settings(
                updated,
                &config,
                &ui,
                &settings,
                tw.upgrade().as_deref(),
                &c,
                &avatar_service::config_path(),
            );
        }
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
            return save_settings(
                updated,
                &config,
                &ui,
                &settings,
                tw.upgrade().as_deref(),
                &c,
                &avatar_service::config_path(),
            );
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
                    use ene_protocol::avatar::MotionState;
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
                    let mut updated = saved.borrow().clone();
                    updated.avatar.right = right;
                    updated.avatar.bottom = bottom;
                    updated.avatar.output = output;
                    match updated.save(&avatar_service::config_path()) {
                        Ok(()) => *saved.borrow_mut() = updated,
                        Err(error) => {
                            ui.set_avatar_status(error.into());
                            c.configure(saved.borrow().avatar.clone());
                        }
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
