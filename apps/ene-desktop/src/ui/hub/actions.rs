//! Chat, audio, device, approval, policy, plugin, remote-host, and companion actions on [`Hub`].

use super::{Hub, companions};
use crate::{
    rpc,
    state::{Chat, NoticeKind},
    update::Command,
};
use ene_client_core::{
    ModelRoleSlot,
    domain::{
        ApprovalDecision, CompanionProfilePatch, CompanionTemplate, MindProfile, NewCompanionSpec,
        PolicyEffect, PolicyRule, PolicyRulesUpdate, PolicyText, PolicyTextsUpdate,
    },
};
use gpui::Context;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RemoteHostError {
    #[error("設定ディレクトリを特定できませんでした。")]
    MissingConfigDir,
    #[error(transparent)]
    Save(#[from] crate::host::Connect),
}

impl Hub {
    /// Queues a command for the session, noting when it has stopped listening.
    ///
    /// The channel closing means the session thread ended, which is expected
    /// during shutdown but worth seeing when it is not.
    pub(super) fn command(&self, command: Command) {
        if self.commands.send(command).is_err() {
            tracing::debug!("the session stopped listening; command dropped");
        }
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
        self.command(Command::Serve(companion_id.to_owned()));
        self.load_history(cx);
        self.load_mind(cx);
        self.windows.pod_scroll_ping = self.windows.pod_scroll_ping.saturating_add(1);
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
        if !rpc::chat::send_message(
            &self.rpc,
            self.model.connections.link_epoch,
            companion,
            request_key,
            text,
        ) {
            // No host is bound, so nothing was queued; the composer keeps the
            // text and the caller's `false` return tells the window to stay.
            tracing::debug!("no host is connected; the message was not queued");
            return false;
        }
        self.windows.pod_scroll_ping = self.windows.pod_scroll_ping.saturating_add(1);
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
        self.command(Command::StopPlayback);
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
        self.command(Command::SetMicMuted(next));
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
        self.command(Command::SetCaptureMode(next));
        cx.notify();
    }

    pub(crate) fn ptt_toggle(&mut self, cx: &mut Context<Self>) {
        if self.model.audio.ptt_active {
            self.model.audio.ptt_active = false;
            self.command(Command::PttCommit);
        } else {
            self.model.audio.ptt_active = true;
            self.command(Command::PttStart);
        }
        cx.notify();
    }

    pub(crate) fn ptt_cancel(&mut self, cx: &mut Context<Self>) {
        self.model.audio.ptt_active = false;
        self.command(Command::PttCancel);
        cx.notify();
    }

    pub(crate) fn set_eyes_enabled(&self, enabled: bool) {
        self.command(Command::SetEyesEnabled(enabled));
    }

    #[cfg(windows)]
    pub(crate) fn set_computer_use_enabled(&self, enabled: bool) {
        self.command(Command::SetComputerUseEnabled(enabled));
    }

    pub(crate) fn toggle_output_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.audio.output_muted;
        self.model.audio.output_muted = next;
        if next {
            self.model.audio.output_rms = 0.0;
        }
        self.command(Command::SetOutputMuted(next));
        self.sync_tray_state();
        cx.notify();
    }

    pub(crate) fn set_avatar_scale(&mut self, scale: f32, cx: &mut Context<Self>) {
        let clamped = scale.clamp(50., 200.);
        self.model.perception.avatar_scale = clamped;
        self.command(Command::SetAvatarScale(clamped));
        cx.notify();
    }

    pub(crate) fn center_avatar(&self) {
        self.command(Command::CenterAvatar);
    }

    /// Speaks typed text through the host's TTS connection, outside any turn.
    pub(crate) fn speak_probe(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.model.audio.speech_report = None;
        let companion_id = self.model.companion_id().map(str::to_owned);
        self.command(Command::Speak { companion_id, text });
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
        self.command(Command::Serve(id));
        cx.notify();
    }

    pub(crate) fn activate_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        self.command(Command::ActivateCompanion(companion_id));
        cx.notify();
    }

    pub(crate) fn release_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        self.command(Command::ReleaseCompanion(companion_id));
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
            && let Err(error) = std::fs::remove_file(&path)
        {
            tracing::warn!(%error, "the remote host config could not be removed");
        }
        cx.notify();
    }

    pub(crate) fn create_companion(&mut self, spec: NewCompanionSpec, cx: &mut Context<Self>) {
        companions::create_companion(self, spec, cx);
    }

    pub(crate) fn update_companion_profile(
        &mut self,
        patch: CompanionProfilePatch,
        cx: &mut Context<Self>,
    ) {
        companions::update_companion_profile(self, patch, cx);
    }

    pub(crate) fn load_companion_templates(&mut self, _cx: &mut Context<Self>) {
        rpc::companions::load_companion_templates(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn export_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        rpc::companions::export_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id,
        );
        cx.notify();
    }

    pub(crate) fn import_companion_json(&mut self, raw: String, cx: &mut Context<Self>) {
        companions::import_companion_json(self, raw, cx);
    }

    pub(crate) fn import_companion_template(
        &mut self,
        template: CompanionTemplate,
        cx: &mut Context<Self>,
    ) {
        companions::import_companion_template(self, template, cx);
    }

    pub(crate) fn preview_companion(
        &mut self,
        name: String,
        mind: String,
        persona: String,
        cx: &mut Context<Self>,
    ) {
        companions::preview_companion(self, name, mind, persona, cx);
    }

    pub(crate) fn assign_model(
        &mut self,
        slot: ModelRoleSlot,
        assignment: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        companions::assign_model(self, slot, assignment, cx);
    }

    pub(crate) fn delete_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        rpc::companions::delete_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id,
        );
        cx.notify();
    }

    pub(crate) fn load_mind(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.load_mind_for(companion_id);
    }

    pub(crate) fn load_mind_for(&mut self, companion_id: String) {
        rpc::companions::load_mind(&self.rpc, self.model.connections.link_epoch, companion_id);
    }

    pub(crate) fn update_mind_profile(
        &mut self,
        companion_id: String,
        expected_revision: u64,
        profile: MindProfile,
        cx: &mut Context<Self>,
    ) {
        companions::update_mind_profile(self, companion_id, expected_revision, profile, cx);
    }
}
