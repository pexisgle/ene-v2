use super::*;

impl Application {
    pub fn toggle_avatar(&self, ui: &AppWindow) {
        let mut updated = self.settings.borrow().saved.clone();
        updated.avatar.visible = !updated.avatar.visible;
        self.persist_live(ui, updated);
    }
    pub fn recenter_avatar(&self, ui: &AppWindow) {
        let mut updated = self.settings.borrow().saved.clone();
        updated.avatar.visible = true;
        if self.persist_live(ui, updated)
            && let Some(avatar) = &self.avatar
        {
            avatar.recenter();
        }
    }
    pub(in crate::application) fn restart_avatar(&self, ui: &AppWindow) {
        if ui.global::<UiState>().get_avatar_failed()
            && let Some(avatar) = &self.avatar
        {
            ui.global::<UiState>().set_avatar_ready(false);
            ui.global::<UiState>()
                .set_avatar_status("描画を再起動しています…".into());
            avatar.restart(self.settings.borrow().saved.avatar.clone());
        }
    }
    pub(in crate::application) fn drain_avatar(&self, ui: &AppWindow) {
        let Some(avatar) = &self.avatar else {
            return;
        };
        for event in avatar.events.try_iter() {
            self.handle_avatar_event(ui, event);
        }
    }
    pub fn handle_avatar_event(&self, ui: &AppWindow, event: AvatarEvent) {
        let view = ui.global::<UiState>();
        match event {
            AvatarEvent::Ready { .. } => {
                view.set_avatar_ready(true);
                view.set_avatar_failed(false);
                view.set_avatar_status("キャラクターを表示しています。".into());
            }
            AvatarEvent::Error { message } => {
                view.set_avatar_ready(false);
                view.set_avatar_failed(true);
                view.set_avatar_status(message.into());
            }
            AvatarEvent::MotionError { message } => {
                view.set_motion_failed(true);
                view.set_motion_status(message.into());
            }
            AvatarEvent::MotionState { state } => {
                let text = match state {
                    MotionState::None if self.settings.borrow().saved.avatar.motion.is_some() => {
                        None
                    }
                    MotionState::None => Some("静止姿勢"),
                    MotionState::Loading => Some("読み込み中…"),
                    MotionState::Playing => Some("再生中"),
                    MotionState::Paused => Some("一時停止中"),
                    MotionState::Finished => Some("再生終了"),
                };
                if let Some(text) = text {
                    view.set_motion_failed(false);
                    view.set_motion_status(text.into());
                }
            }
            AvatarEvent::ShowChat => navigate(ui, 0),
            AvatarEvent::PositionChanged {
                right,
                bottom,
                output,
            } => {
                let mut updated = self.settings.borrow().saved.clone();
                updated.avatar.right = right;
                updated.avatar.bottom = bottom;
                updated.avatar.output = output;
                // The renderer already moved; acknowledge without reconfiguring it.
                match updated.save(&self.path) {
                    Ok(()) => {
                        self.settings.borrow_mut().accept_live(updated);
                        self.publish_settings(ui);
                    }
                    Err(error) => {
                        view.set_avatar_status(error.into());
                        if let Some(avatar) = &self.avatar {
                            avatar.configure(self.settings.borrow().saved.avatar.clone());
                        }
                    }
                }
            }
            AvatarEvent::VisibilityChanged { visible } => {
                if !view.get_avatar_failed() {
                    view.set_avatar_status(
                        if visible {
                            "キャラクターを表示しています。"
                        } else {
                            "キャラクターは非表示です。"
                        }
                        .into(),
                    );
                }
                if let Some(tray) = &self.tray {
                    tray.set_avatar_visible(visible);
                }
            }
            AvatarEvent::Stopped => view.set_avatar_ready(false),
        }
        if let Some(tray) = &self.tray {
            tray.set_avatar_failed(view.get_avatar_failed());
        }
    }
}
