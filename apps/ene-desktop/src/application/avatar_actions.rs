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
        self.persist_live(ui, updated);
        if let Some(avatar) = &self.avatar {
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
            avatar.restart();
        }
    }
    pub fn shutdown(&self, ui: &AppWindow) {
        if let Some(avatar) = &self.avatar {
            avatar.shutdown();
        }
        self.drain_avatar(ui);
        self.saver.shutdown();
        self.drain_avatar(ui);
    }
    pub(in crate::application) fn drain_avatar(&self, ui: &AppWindow) {
        let view = ui.global::<UiState>();
        if let Some(avatar) = &self.avatar {
            let notifications = avatar.drain();
            if let Some(state) = notifications.state {
                if self.settings.borrow().saved.placement != state.placement {
                    let mut updated = self.settings.borrow().saved.clone();
                    updated.placement = state.placement;
                    self.settings.borrow_mut().accept_live(updated.clone());
                    self.saver.update(updated);
                    view.set_save_status("保存待ち…".into());
                }
                let visible = self.settings.borrow().saved.avatar.visible;
                view.set_avatar_ready(visible && !state.loading && state.error.is_none());
                view.set_avatar_failed(state.error.is_some());
                view.set_avatar_status(if let Some(error) = state.error {
                    error.into()
                } else if !visible {
                    "キャラクターは非表示です。".into()
                } else if state.loading {
                    "キャラクターを読み込んでいます…".into()
                } else {
                    format!("表示中 · {}", state.model.as_deref().unwrap_or("")).into()
                });
                view.set_motion_failed(matches!(state.motion, MotionState::Failed { .. }));
                view.set_motion_status(match state.motion {
                    MotionState::None => "静止姿勢".into(),
                    MotionState::Loading => "読み込み中…".into(),
                    MotionState::Playing => "再生中".into(),
                    MotionState::Paused => "一時停止中".into(),
                    MotionState::Finished => "再生終了".into(),
                    MotionState::Failed { message } => message.into(),
                });
            }
            for event in notifications.events {
                match event {
                    AvatarEvent::MotionFinished => {}
                }
            }
            if let Some(error) = notifications.error {
                view.set_avatar_failed(true);
                view.set_avatar_ready(false);
                view.set_avatar_status(error.into());
            }
        }
        if let Some(result) = self.saver.take_result() {
            if let Some(error) = result.error {
                view.set_save_failed(true);
                view.set_save_status(format!("未保存 · {error}").into());
            } else if result.settings == self.settings.borrow().saved {
                view.set_save_failed(false);
                view.set_save_status("保存しました。".into());
            }
        }
        if let Some(tray) = &self.tray {
            tray.set_avatar_failed(view.get_avatar_failed());
        }
    }
}
