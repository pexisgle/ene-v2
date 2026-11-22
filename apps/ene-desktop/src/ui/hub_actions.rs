//! Chat, audio, device, approval, policy, plugin, and remote-host actions on [`Hub`].

use super::Hub;
use crate::{
    rpc,
    state::{Chat, Effects, NoticeKind},
    update::{Command, Update},
};
use ene_client_core::domain::{
    ApprovalDecision, ConnectionKind, NotificationEvent, PolicyEffect, PolicyRule,
    PolicyRulesUpdate, PolicyText, PolicyTextsUpdate,
};
use gpui::Context;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteHostError {
    #[error("設定ディレクトリを特定できませんでした。")]
    MissingConfigDir,
    #[error(transparent)]
    Save(#[from] crate::host::Connect),
}

pub(super) struct ApplyFlags {
    pub(super) conflicted: bool,
    pub(super) history_arrived: bool,
    pub(super) settings_changed: bool,
    pub(super) companion_or_snapshot: bool,
    pub(super) models_or_check: bool,
    pub(super) message_arrived: bool,
}

impl ApplyFlags {
    fn from_update(update: &Update) -> Self {
        let notification_event = match update {
            Update::Notification(notification) => notification.event.as_ref(),
            _ => None,
        };
        Self {
            conflicted: matches!(update, Update::Conflict { .. }),
            history_arrived: matches!(update, Update::History { .. }),
            settings_changed: matches!(
                notification_event,
                Some(NotificationEvent::SettingsUpdated(_))
            ),
            companion_or_snapshot: matches!(
                notification_event,
                Some(
                    NotificationEvent::InitialState(_)
                        | NotificationEvent::Companion(_)
                        | NotificationEvent::SettingsUpdated(_)
                )
            ),
            models_or_check: matches!(
                update,
                Update::Providers(_) | Update::Models { .. } | Update::Checked { .. }
            ),
            message_arrived: matches!(
                notification_event,
                Some(NotificationEvent::MessageCreated(_) | NotificationEvent::MessageDelta(_))
            ),
        }
    }
}

impl Hub {
    pub(super) fn pre_apply_hub_state(
        &mut self,
        update: &Update,
        cx: &mut Context<Self>,
    ) -> ApplyFlags {
        self.apply_ui_update_bookkeeping(update, cx);
        if let Update::Checked {
            connection_id: _,
            check,
        } = update
            && check.healthy
        {
            self.load_models(cx);
        }
        if let Update::Failed { context, .. }
        | Update::Conflict { context, .. }
        | Update::Denied { context, .. } = update
            && *context == "接続確認"
        {
            self.checking_connections.clear();
        }
        if let Update::SendFailed { reason, .. } = update {
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        ApplyFlags::from_update(update)
    }

    fn apply_ui_update_bookkeeping(&mut self, update: &Update, cx: &mut Context<Self>) {
        match update {
            Update::CredentialRegistered { connection_id } => {
                let next_seq = self
                    .cleared_secret_for
                    .as_ref()
                    .map_or(1, |(_, seq)| seq + 1);
                self.cleared_secret_for = Some((connection_id.clone(), next_seq));
            }
            Update::Checked {
                connection_id: id,
                check,
            } => {
                self.checking_connections.remove(id);
                if check.healthy
                    && self.model.settings().is_some_and(|s| {
                        s.connections
                            .iter()
                            .any(|c| &c.id == id && c.kind == ConnectionKind::Llm)
                    })
                {
                    self.setup_error = None;
                    self.setup_error_detail = None;
                }
            }
            Update::SetupFinished {
                ok,
                connection_id,
                models,
                error,
            } => {
                self.setup_pending = false;
                if *ok {
                    self.setup_error = None;
                    self.setup_error_detail = None;
                    self.pending_setup_connection_id = connection_id.clone();
                    self.pending_setup_models = models.clone();
                } else {
                    self.setup_error = Some(
                        "接続を確認できませんでした。選んだサービスのキーとネットワークを確認して、もう一度登録してください。",
                    );
                    self.setup_error_detail = error.clone();
                }
                self.resnapshot();
                if *ok {
                    self.load_models(cx);
                }
            }
            Update::Resnapshot => self.resnapshot(),
            Update::Created(id) => {
                let created_id = id.clone();
                self.created_ping = self.created_ping.saturating_add(1);
                self.resnapshot();
                self.select_companion(&created_id, cx);
            }
            _ => {}
        }
    }

    pub(super) fn dispatch_apply_effects(
        &mut self,
        flags: ApplyFlags,
        effects: Effects,
        had_companion: bool,
        had_approvals: usize,
        cx: &mut Context<Self>,
    ) {
        let selected_now = !had_companion && self.model.companion_id().is_some();
        if flags.settings_changed {
            let _ = self.commands.send(Command::Refresh);
            self.load_models(cx);
        }
        if flags.companion_or_snapshot || flags.models_or_check || selected_now {
            self.maybe_auto_assign_default_model();
        }
        if flags.conflicted {
            self.resnapshot();
            self.refresh_active_data(cx);
        }
        if self.model.approvals().len() > had_approvals
            || flags.message_arrived
            || flags.history_arrived
        {
            self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        }
        if effects.history || selected_now {
            self.load_history(cx);
        }
        if effects.approvals || selected_now {
            self.load_approvals(cx);
        }
        if effects.mind || selected_now {
            self.load_mind(cx);
        }
        if self.live() && self.model.connections.providers.is_none() {
            self.load_providers(cx);
        }
        if selected_now {
            self.load_models(cx);
            self.load_policy_rules(cx);
            self.load_policy_texts(cx);
        }
        self.sync_tray_state();
        cx.notify();
    }

    fn maybe_auto_assign_default_model(&mut self) {
        if !self.live() || self.setup_pending || self.pending_setup_connection_id.is_some() {
            return;
        }
        let Some(patch) = self
            .model
            .auto_assign_default_model_request(&self.auto_assigned_revisions)
        else {
            return;
        };
        if self.rpc.host().is_none() {
            return;
        }
        self.auto_assigned_revisions
            .insert(patch.companion_id.clone(), patch.revision);
        rpc::companions::auto_assign_default_model(
            &self.rpc,
            self.model.connections.link_epoch,
            patch,
        );
    }

    pub(crate) fn refresh_active_data(&mut self, cx: &mut Context<Self>) {
        self.load_history(cx);
        self.load_approvals(cx);
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
        self.load_policy_rules(cx);
        self.load_policy_texts(cx);
    }

    pub(crate) fn resnapshot(&self) {
        rpc::chat::resnapshot(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn select_companion(&mut self, companion_id: &str, cx: &mut Context<Self>) {
        if self.model.companion_id() == Some(companion_id) {
            return;
        }
        self.model.companions.selected = Some(companion_id.to_owned());
        self.model.companions.chat = Chat {
            companion_id: companion_id.to_owned(),
            loading: true,
            ..Default::default()
        };
        self.model.companions.streaming.clear();
        self.model.audio.transcription = None;
        let _ = self.commands.send(Command::Serve(companion_id.to_owned()));
        self.load_history(cx);
        self.load_mind(cx);
        self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        cx.notify();
    }

    pub(crate) fn load_history(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        if !self.live() {
            self.model.companions.chat.loading = false;
            return;
        }
        if !rpc::chat::load_history(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id.clone(),
        ) {
            return;
        }
        if self.model.companions.chat.companion_id != companion_id {
            self.model.companions.chat = Chat {
                companion_id,
                loading: true,
                ..Default::default()
            };
        } else {
            self.model.companions.chat.loading = true;
        }
    }

    pub(crate) fn send(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let request_key = uuid::Uuid::new_v4().to_string();
        self.send_keyed(text, request_key, cx)
    }

    pub(crate) fn send_keyed(
        &mut self,
        text: String,
        request_key: String,
        cx: &mut Context<Self>,
    ) -> bool {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return false;
        }
        if !self.live() {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "ホストにまだ接続されていません。",
            );
            cx.notify();
            return false;
        }
        let Some(companion) = self.model.companion_id().map(str::to_owned) else {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "会話するコンパニオンが選択されていません。",
            );
            cx.notify();
            return false;
        };
        if self.rpc.host().is_none() {
            return false;
        }
        self.model
            .companions
            .chat
            .sending
            .push(crate::state::Sending {
                request_key: request_key.clone(),
                text: text.clone(),
            });
        tracing::info!(
            companion = %companion,
            key = %request_key,
            chars = text.chars().count(),
            "message sent"
        );
        let _ = rpc::chat::send_message(
            &self.rpc,
            self.model.connections.link_epoch,
            companion,
            request_key,
            text,
        );
        self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        cx.notify();
        true
    }

    pub(crate) fn retry_unsent(&mut self, request_key: &str, text: &str, cx: &mut Context<Self>) {
        self.model
            .companions
            .chat
            .unsent
            .retain(|item| item.request_key != request_key);
        let _ = self.send_keyed(text.to_owned(), request_key.to_owned(), cx);
    }

    pub(crate) fn dismiss_unsent(&mut self, request_key: &str, cx: &mut Context<Self>) {
        self.model
            .companions
            .chat
            .unsent
            .retain(|item| item.request_key != request_key);
        cx.notify();
    }

    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::StopPlayback);
        if let Some(companion) = self.model.companion_id().map(str::to_owned) {
            rpc::chat::stop(&self.rpc, self.model.connections.link_epoch, companion);
        }
        cx.notify();
    }

    pub(crate) fn toggle_mic_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.audio.mic_muted;
        self.model.audio.mic_muted = next;
        if next {
            self.model.audio.input_rms = 0.0;
            self.model.audio.transcription = None;
        }
        let _ = self.commands.send(Command::SetMicMuted(next));
        self.sync_tray_state();
        cx.notify();
    }

    pub(crate) fn toggle_capture_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.model.audio.capture_mode {
            crate::audio::CaptureMode::Vad => crate::audio::CaptureMode::Ptt,
            crate::audio::CaptureMode::Ptt => crate::audio::CaptureMode::Vad,
        };
        self.model.audio.capture_mode = next;
        self.model.audio.ptt_active = false;
        let _ = self.commands.send(Command::SetCaptureMode(next));
        cx.notify();
    }

    pub(crate) fn ptt_toggle(&mut self, cx: &mut Context<Self>) {
        if self.model.audio.ptt_active {
            self.model.audio.ptt_active = false;
            let _ = self.commands.send(Command::PttCommit);
        } else {
            self.model.audio.ptt_active = true;
            let _ = self.commands.send(Command::PttStart);
        }
        cx.notify();
    }

    pub(crate) fn ptt_cancel(&mut self, cx: &mut Context<Self>) {
        self.model.audio.ptt_active = false;
        let _ = self.commands.send(Command::PttCancel);
        cx.notify();
    }

    pub(crate) fn set_eyes_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::SetEyesEnabled(enabled));
    }

    #[cfg(windows)]
    pub(crate) fn set_computer_use_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::SetComputerUseEnabled(enabled));
    }

    pub(crate) fn toggle_output_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.audio.output_muted;
        self.model.audio.output_muted = next;
        if next {
            self.model.audio.output_rms = 0.0;
        }
        let _ = self.commands.send(Command::SetOutputMuted(next));
        self.sync_tray_state();
        cx.notify();
    }

    pub(crate) fn set_avatar_scale(&mut self, scale: f32, cx: &mut Context<Self>) {
        let clamped = scale.clamp(50., 200.);
        self.model.perception.avatar_scale = clamped;
        let _ = self.commands.send(Command::SetAvatarScale(clamped));
        cx.notify();
    }

    pub(crate) fn center_avatar(&self) {
        let _ = self.commands.send(Command::CenterAvatar);
    }

    /// Speaks typed text through the host's TTS connection, outside any turn.
    pub(crate) fn speak_probe(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.model.audio.speech_report = None;
        let companion_id = self.model.companion_id().map(str::to_owned);
        let _ = self.commands.send(Command::Speak { companion_id, text });
        cx.notify();
    }

    /// Forgets the speech probe's readback and recognized lines.
    pub(crate) fn clear_voice_probe(&mut self, cx: &mut Context<Self>) {
        self.model.audio.recognized.clear();
        self.model.audio.speech_report = None;
        cx.notify();
    }

    pub(crate) fn activate_this_client(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.model.companions.selected = Some(id.clone());
        let _ = self.commands.send(Command::Serve(id));
        cx.notify();
    }

    pub(crate) fn activate_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::ActivateCompanion(companion_id));
        cx.notify();
    }

    pub(crate) fn release_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::ReleaseCompanion(companion_id));
        cx.notify();
    }

    pub(crate) fn dismiss_notice(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.model.notices.len() {
            self.model.notices.remove(index);
            cx.notify();
        }
    }

    pub(crate) fn load_approvals(&mut self, _cx: &mut Context<Self>) {
        rpc::policy::load_approvals(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        rpc::policy::resolve_approval(
            &self.rpc,
            self.model.connections.link_epoch,
            request_id.to_owned(),
            decision,
        );
        cx.notify();
    }

    pub(crate) fn load_policy_rules(&mut self, _cx: &mut Context<Self>) {
        rpc::policy::load_policy_rules(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn add_policy_rule(
        &mut self,
        tool_name: String,
        pattern: String,
        effect: PolicyEffect,
        cx: &mut Context<Self>,
    ) {
        let rule = PolicyRule {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name,
            pattern,
            effect,
            ..Default::default()
        };
        self.update_policy_rules(
            "許可ルールの追加",
            PolicyRulesUpdate {
                add_or_update: vec![rule],
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn remove_policy_rule(&mut self, id: String, cx: &mut Context<Self>) {
        self.update_policy_rules(
            "許可ルールの削除",
            PolicyRulesUpdate {
                remove_ids: vec![id],
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn update_policy_rules(
        &mut self,
        context: &'static str,
        update: PolicyRulesUpdate,
        cx: &mut Context<Self>,
    ) {
        rpc::policy::update_policy_rules(
            &self.rpc,
            self.model.connections.link_epoch,
            context,
            update,
        );
        cx.notify();
    }

    pub(crate) fn load_policy_texts(&mut self, _cx: &mut Context<Self>) {
        rpc::policy::load_policy_texts(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn upsert_policy_text(
        &mut self,
        id: Option<String>,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let entry = PolicyText {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            text,
            ..Default::default()
        };
        self.update_policy_texts(
            "自由文ポリシーの保存",
            PolicyTextsUpdate {
                add_or_update: vec![entry],
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn remove_policy_text(&mut self, id: String, cx: &mut Context<Self>) {
        self.update_policy_texts(
            "自由文ポリシーの削除",
            PolicyTextsUpdate {
                remove_ids: vec![id],
                ..Default::default()
            },
            cx,
        );
    }

    fn update_policy_texts(
        &mut self,
        context: &'static str,
        update: PolicyTextsUpdate,
        cx: &mut Context<Self>,
    ) {
        rpc::policy::update_policy_texts(
            &self.rpc,
            self.model.connections.link_epoch,
            context,
            update,
        );
        cx.notify();
    }

    pub(crate) fn load_plugins(&mut self, _cx: &mut Context<Self>) {
        rpc::policy::load_plugins(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn install_plugin(&mut self, source_path: String, cx: &mut Context<Self>) {
        let trimmed = source_path.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        rpc::policy::install_plugin(&self.rpc, self.model.connections.link_epoch, trimmed);
        cx.notify();
    }

    pub(crate) fn set_plugin_enabled(
        &mut self,
        plugin_id: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        rpc::policy::set_plugin_enabled(
            &self.rpc,
            self.model.connections.link_epoch,
            plugin_id,
            enabled,
        );
        cx.notify();
    }

    pub(crate) fn remove_plugin(&mut self, plugin_id: String, cx: &mut Context<Self>) {
        rpc::policy::remove_plugin(&self.rpc, self.model.connections.link_epoch, plugin_id);
        cx.notify();
    }

    pub(crate) fn remote_host_config_path() -> Option<std::path::PathBuf> {
        directories::ProjectDirs::from("", "", "ene")
            .map(|dirs| dirs.config_dir().join("remote_host.json"))
    }

    pub(crate) fn current_remote_host() -> Option<String> {
        let path = Self::remote_host_config_path()?;
        match crate::host::HostTarget::read_remote(&path) {
            Ok(Some(crate::host::HostTarget::Remote { endpoint, .. })) => Some(endpoint),
            _ => None,
        }
    }

    pub(crate) fn save_remote_host(
        &mut self,
        endpoint: String,
        token: String,
        cx: &mut Context<Self>,
    ) -> Result<(), RemoteHostError> {
        let Some(path) = Self::remote_host_config_path() else {
            return Err(RemoteHostError::MissingConfigDir);
        };
        crate::host::HostTarget::write_remote(&path, endpoint.trim(), token.trim())?;
        self.model.notice(
            NoticeKind::Failed,
            "遠隔ホスト設定を保存しました",
            format!(
                "{} を保存しました。アプリ再起動時に遠隔 HTTPS ホストへ接続できます。",
                endpoint.trim()
            ),
        );
        cx.notify();
        Ok(())
    }

    pub(crate) fn clear_remote_host(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = Self::remote_host_config_path()
            && path.exists()
        {
            let _ = std::fs::remove_file(path);
        }
        cx.notify();
    }

    pub(crate) fn tts_voice_choices(&self) -> Vec<(String, String, String, String)> {
        self.model.tts_voice_choices()
    }

    pub(crate) fn voice_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.voice_choices_for_connection(conn_id)
    }

    pub(crate) fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.stt_model_choices_for_connection(conn_id)
    }

    pub(crate) fn embedding_model_choices_for_connection(
        &self,
        conn_id: &str,
    ) -> Vec<(String, String)> {
        self.model.embedding_model_choices_for_connection(conn_id)
    }

    pub(crate) fn available_models_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        self.model.available_models_for_connection(conn_id)
    }

    pub(crate) fn model_choices(&self) -> Vec<(String, String, String)> {
        self.model.model_choices()
    }

    pub(crate) fn all_llm_models(&self) -> Vec<(String, String, String)> {
        self.model.all_llm_models()
    }

    pub(crate) fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.model.all_embedding_models()
    }

    pub(crate) fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.model.all_stt_models()
    }

    pub(crate) fn active_stt_selection(&self) -> Option<(String, String)> {
        self.model.active_stt_selection()
    }

    pub(crate) fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.model.all_tts_voices()
    }

    pub(crate) fn active_tts_selection(&self) -> Option<(String, String)> {
        self.model.active_tts_selection()
    }
}
