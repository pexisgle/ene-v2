//! Pure state reducer folding [`Update`] messages and host notifications into [`Model`].

use crate::{
    state::{
        AudioState, Chat, Effects, Link, LiveTranscription, Model, NoticeKind, PerceptionState,
        SpeechReport, Unsent,
    },
    update::Update,
};
use ene_rpc as w;

impl Model {
    /// Folds one update into the view and reports what must be refetched.
    pub fn apply(&mut self, update: Update) -> Effects {
        let mut effects = Effects::default();
        match update {
            Update::Scoped {
                epoch,
                seq: _,
                inner,
            } => {
                if epoch != self.connections.link_epoch {
                    return Effects::default();
                }
                return self.apply(*inner);
            }
            Update::Link(link) => apply_link(self, *link, &mut effects),
            Update::Notification(notification) => {
                merge_notification(self, *notification, &mut effects);
            }
            Update::Resync => self.companions.streaming.clear(),
            Update::Created(_)
            | Update::SetupFinished { .. }
            | Update::CredentialRegistered { .. }
            | Update::Resnapshot => {}
            Update::Devices(devices) => self.connections.devices = *devices,
            Update::Failed { context, message } => {
                self.notice(NoticeKind::Failed, context, message)
            }
            Update::Conflict { context, message } => {
                self.notice(NoticeKind::Conflict, context, message);
            }
            Update::Denied { context, message } => {
                self.notice(NoticeKind::Denied, context, message)
            }
            Update::Providers(providers) => self.connections.providers = Some(providers),
            Update::Models {
                connection_id,
                models,
            } => {
                self.connections.models.insert(connection_id, models);
            }
            Update::Checked {
                connection_id,
                check,
            } => {
                self.connections.checks.insert(connection_id, check);
            }
            Update::Plugins(plugins) => self.connections.plugins = plugins,
            other => apply_domain_update(self, other, &mut effects),
        }
        effects
    }
}

fn apply_link(model: &mut Model, link: Link, effects: &mut Effects) {
    let was_live = model.connections.link.is_live();
    let is_live = link.is_live();
    let appeared = is_live && !was_live;
    if was_live != is_live {
        model.connections.link_epoch = model.connections.link_epoch.saturating_add(1);
    }
    model.connections.link = link;
    if !model.connections.link.is_live() {
        model.companions.streaming.clear();
        model.audio.companion_speaking = false;
        model.audio.output_rms = 0.0;
        model.audio.transcription = None;
        for sending in std::mem::take(&mut model.companions.chat.sending) {
            model.companions.chat.unsent.push(Unsent {
                request_key: sending.request_key,
                text: sending.text,
                reason: "接続が切れたため結果不明です。再送しても二重にはなりません。".into(),
            });
        }
    }
    if appeared {
        effects.history = true;
        effects.approvals = true;
    }
}

fn apply_domain_update(model: &mut Model, update: Update, effects: &mut Effects) {
    match update {
        Update::Accepted { .. }
        | Update::SendFailed { .. }
        | Update::History { .. }
        | Update::HistoryFailed { .. }
        | Update::StopRequested { .. }
        | Update::Gone { .. }
        | Update::MindLoaded { .. }
        | Update::CompanionTemplates(_)
        | Update::CompanionExported { .. }
        | Update::CompanionPreview { .. } => apply_companion_update(model, update),
        Update::Approvals(_)
        | Update::Resolved { .. }
        | Update::ApprovalStale { .. }
        | Update::PolicyRules(_)
        | Update::PolicyTexts(_) => apply_approval_update(model, update, effects),
        Update::AudioLevel { .. }
        | Update::AudioMute { .. }
        | Update::CaptureModeChanged { .. }
        | Update::Transcription { .. }
        | Update::Capture { .. }
        | Update::Spoke
        | Update::SpokeFailed { .. } => apply_audio_update(&mut model.audio, update),
        Update::EyesEnabled(_) | Update::EyesStatus(_) | Update::AvatarScaleChanged(_) => {
            apply_perception_update(&mut model.perception, update);
        }
        #[cfg(windows)]
        Update::ComputerUseEnabled(_) => apply_perception_update(&mut model.perception, update),
        _ => {}
    }
}

fn apply_companion_update(model: &mut Model, update: Update) {
    let companions = &mut model.companions;
    match update {
        Update::Accepted { request_key, .. } => {
            companions
                .chat
                .sending
                .retain(|sending| sending.request_key != request_key);
        }
        Update::SendFailed {
            request_key,
            text,
            reason,
        } => {
            companions
                .chat
                .sending
                .retain(|sending| sending.request_key != request_key);
            companions.chat.unsent.push(Unsent {
                request_key,
                text,
                reason,
            });
        }
        Update::History {
            companion_id,
            messages,
        } => apply_history(&mut companions.chat, &companion_id, messages),
        Update::HistoryFailed {
            companion_id,
            message,
        } => {
            if companions.chat.companion_id == companion_id {
                companions.chat.loading = false;
                companions.chat.error = Some(message);
            }
        }
        Update::StopRequested { companion_id } => {
            if companions.chat.companion_id == companion_id {
                companions.chat.stop_requested = true;
            }
        }
        Update::Gone { companion_id } => remove_companion(model, &companion_id),
        Update::MindLoaded { companion_id, view } => {
            companions.minds.insert(companion_id, *view);
        }
        Update::CompanionTemplates(items) => companions.templates = items,
        Update::CompanionExported { companion_id, json } => {
            companions.exported_template = Some((companion_id, json));
        }
        Update::CompanionPreview { reply, error } => {
            companions.preview_reply = Some(match reply {
                Some(text) => Ok(text),
                None => Err(crate::state::PreviewError(
                    error.unwrap_or_else(|| "プレビューに失敗しました".into()),
                )),
            });
        }
        _ => {}
    }
}

fn apply_history(chat: &mut Chat, companion_id: &str, messages: Vec<w::Message>) {
    if chat.companion_id != companion_id {
        return;
    }
    chat.messages = messages
        .into_iter()
        .filter(is_user_facing_message)
        .collect();
    chat.loading = false;
    chat.error = None;
    chat.sending.retain(|sending| {
        !chat
            .messages
            .iter()
            .any(|message| message.role == w::MessageRole::User && message.text == sending.text)
    });
}

fn remove_companion(model: &mut Model, companion_id: &str) {
    if let Some(snapshot) = model.snapshot.as_mut() {
        snapshot
            .companions
            .retain(|companion| companion.id != companion_id);
        snapshot
            .activations
            .retain(|activation| activation.companion_id != companion_id);
    }
    model.companions.minds.remove(companion_id);
    if model.companions.selected.as_deref() == Some(companion_id) {
        model.companions.selected = None;
        model.companions.chat = Chat::default();
        model.companions.streaming.clear();
    }
}

fn apply_approval_update(model: &mut Model, update: Update, effects: &mut Effects) {
    match update {
        Update::Approvals(items) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.pending_approvals = items;
            }
        }
        Update::Resolved {
            request_id,
            rule_warning,
        } => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot
                    .pending_approvals
                    .retain(|approval| approval.request_id != request_id);
            }
            model.approval.last_resolved_id = Some(request_id);
            model.approval.last_rule_warning = rule_warning.clone();
            if let Some(warning) = rule_warning {
                model.notice(NoticeKind::Failed, "許可ルールの保存", warning);
            }
        }
        Update::ApprovalStale { message } => {
            model.notice(NoticeKind::Conflict, "承認", message);
            effects.approvals = true;
        }
        Update::PolicyRules(rules) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.policy_rules = rules;
            }
        }
        Update::PolicyTexts(texts) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                let mut settings = snapshot.settings.take().unwrap_or_default();
                settings.policy_texts = texts;
                snapshot.settings = Some(settings).into();
            }
        }
        _ => {}
    }
}

fn apply_audio_update(audio: &mut AudioState, update: Update) {
    match update {
        Update::AudioLevel {
            output,
            input,
            speaking,
        } => {
            audio.output_rms = output;
            audio.input_rms = input;
            audio.companion_speaking = speaking;
        }
        Update::AudioMute {
            mic_muted,
            output_muted,
        } => {
            audio.mic_muted = mic_muted;
            audio.output_muted = output_muted;
            if mic_muted {
                audio.input_rms = 0.0;
                audio.transcription = None;
            }
            if output_muted {
                audio.output_rms = 0.0;
            }
        }
        Update::CaptureModeChanged { mode, ptt_active } => {
            audio.capture_mode = mode;
            audio.ptt_active = ptt_active;
        }
        Update::Transcription {
            text,
            is_final,
            is_speaking,
        } => apply_transcription(audio, text, is_final, is_speaking),
        Update::Capture { level, open, sent } => {
            audio.capture = crate::state::Capture { level, open, sent };
        }
        Update::Spoke => {
            audio.speech_report = Some(SpeechReport {
                text: "読み上げを送りました。音が鳴らない場合は、この端末の出力と読み上げ接続を確認してください。".into(),
                failed: false,
            });
        }
        Update::SpokeFailed { message } => {
            audio.speech_report = Some(SpeechReport {
                text: message,
                failed: true,
            });
        }
        _ => {}
    }
}

fn apply_transcription(audio: &mut AudioState, text: String, is_final: bool, is_speaking: bool) {
    if is_final {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            audio.recognized.push(trimmed.to_owned());
            drop(
                audio
                    .recognized
                    .split_off(audio.recognized.len().saturating_sub(20)),
            );
        }
    }
    if is_final || (text.trim().is_empty() && !is_speaking) {
        audio.transcription = None;
    } else {
        audio.transcription = Some(LiveTranscription {
            text,
            is_final,
            is_speaking,
        });
    }
}

fn apply_perception_update(perception: &mut PerceptionState, update: Update) {
    match update {
        Update::EyesEnabled(enabled) => {
            perception.eyes_enabled = enabled;
            perception.eyes_status = None;
        }
        Update::EyesStatus(status) => perception.eyes_status = status,
        #[cfg(windows)]
        Update::ComputerUseEnabled(enabled) => perception.computer_use_enabled = enabled,
        Update::AvatarScaleChanged(scale) => perception.avatar_scale = scale,
        _ => {}
    }
}

/// Whether a host message should be visible in user-facing chat bubbles.
pub fn is_user_facing_message(message: &w::Message) -> bool {
    if !message.work_id.trim().is_empty() {
        return false;
    }
    if matches!(
        message.role.as_known(),
        Some(w::MessageRole::Agent | w::MessageRole::Context)
    ) {
        return false;
    }
    let trimmed = message.text.trim();
    !(trimmed.starts_with("[Output of ")
        || trimmed.starts_with("[作業報告")
        || trimmed.starts_with("[作業結果")
        || trimmed.starts_with("[hand ")
        || trimmed.starts_with("The hand finished"))
}

fn merge_notification(model: &mut Model, notification: w::Notification, effects: &mut Effects) {
    match notification.event {
        Some(
            event @ (w::notification::Event::InitialState(_)
            | w::notification::Event::Health(_)
            | w::notification::Event::Companion(_)
            | w::notification::Event::SettingsUpdated(_)
            | w::notification::Event::ClientUpdated(_)
            | w::notification::Event::ActivationUpdated(_)),
        ) => merge_snapshot_event(model, event, effects),
        Some(
            event @ (w::notification::Event::MessageCreated(_)
            | w::notification::Event::MessageDelta(_)
            | w::notification::Event::TurnEvent(_)
            | w::notification::Event::ResyncRequired(_)),
        ) => merge_chat_event(model, event),
        Some(
            event @ (w::notification::Event::ApprovalRequested(_)
            | w::notification::Event::PolicyRulesUpdated(_)
            | w::notification::Event::PolicyTextsUpdated(_)),
        ) => merge_approval_event(model, event, effects),
        Some(w::notification::Event::WorkEvent(work)) => merge_work_event(model, &work),
        Some(w::notification::Event::Generic(event)) => merge_generic_notice(model, &event),
        Some(w::notification::Event::MindUpdated(updated)) => {
            effects.mind |= model.companion_id() == Some(updated.companion_id.as_str())
                || (!updated.companion_id.is_empty()
                    && model.companions.chat.companion_id == updated.companion_id);
        }
        Some(
            w::notification::Event::DeviceOperation(_)
            | w::notification::Event::DeviceOperationResult(_),
        )
        | None => {}
    }
}

fn merge_snapshot_event(model: &mut Model, event: w::notification::Event, effects: &mut Effects) {
    match event {
        w::notification::Event::InitialState(snapshot) => {
            let mut snapshot = *snapshot;
            snapshot
                .companions
                .sort_by(|left, right| left.name.cmp(&right.name));
            model.companions.work_active = snapshot
                .work_sessions
                .iter()
                .any(|session| matches!(session.status.as_known(), Some(w::WorkStatus::Active)));
            model.snapshot = Some(snapshot);
            effects.approvals = false;
        }
        w::notification::Event::Health(health) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.health = Some(*health).into();
            }
        }
        w::notification::Event::Companion(companion) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_by_revision(
                    &mut snapshot.companions,
                    *companion,
                    |item| item.id.clone(),
                    |item| item.revision,
                );
                snapshot
                    .companions
                    .sort_by(|left, right| left.name.cmp(&right.name));
            }
        }
        w::notification::Event::SettingsUpdated(updated) => {
            if let Some(settings) = updated.settings.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
                && snapshot
                    .settings
                    .as_option()
                    .is_none_or(|current| current.revision <= settings.revision)
            {
                snapshot.settings = Some(settings).into();
            }
        }
        w::notification::Event::ClientUpdated(updated) => {
            if let Some(client) = updated.client.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_by_revision(
                    &mut snapshot.clients,
                    client,
                    |item| item.id.clone(),
                    |item| item.revision,
                );
            }
        }
        w::notification::Event::ActivationUpdated(activation) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_activation(&mut snapshot.activations, *activation);
            }
        }
        _ => {}
    }
}

fn merge_chat_event(model: &mut Model, event: w::notification::Event) {
    let companions = &mut model.companions;
    match event {
        w::notification::Event::MessageCreated(created) => {
            if let Some(message) = created.message.into_option()
                && message.companion_id == companions.chat.companion_id
                && is_user_facing_message(&message)
            {
                companions
                    .streaming
                    .retain(|(companion, _), _| companion != &message.companion_id);
                merge_message(&mut companions.chat.messages, message);
            }
        }
        w::notification::Event::MessageDelta(delta) => {
            if delta.companion_id == companions.chat.companion_id {
                let text = companions
                    .streaming
                    .entry((delta.companion_id.clone(), delta.stream_id.clone()))
                    .or_default();
                text.push_str(&delta.delta);
            }
        }
        w::notification::Event::TurnEvent(turn) => merge_turn_event(model, *turn),
        w::notification::Event::ResyncRequired(_) => companions.streaming.clear(),
        _ => {}
    }
}

fn merge_turn_event(model: &mut Model, turn: w::TurnEvent) {
    let companions = &mut model.companions;
    if turn.companion_id != companions.chat.companion_id {
        return;
    }
    let terminal = matches!(
        turn.status.as_known(),
        Some(w::TurnStatus::Answered | w::TurnStatus::Stopped | w::TurnStatus::Failed)
    );
    if terminal {
        companions.streaming.clear();
        companions.chat.stop_requested = false;
    }
    let failed_message = if turn.status.as_known() == Some(w::TurnStatus::Failed) {
        turn.failure
            .as_option()
            .map(|failure| failure.message.clone())
            .filter(|message| !message.trim().is_empty())
    } else {
        None
    };
    companions.chat.turn = Some(turn);
    if let Some(message) = failed_message {
        model.notice(NoticeKind::Failed, "応答に失敗しました", message);
    }
}

fn merge_approval_event(model: &mut Model, event: w::notification::Event, effects: &mut Effects) {
    match event {
        w::notification::Event::ApprovalRequested(requested) => {
            if let Some(request) = requested.request.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_approval(&mut snapshot.pending_approvals, request);
            } else {
                effects.approvals = true;
            }
        }
        w::notification::Event::PolicyRulesUpdated(updated) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.policy_rules = updated.rules;
            }
        }
        w::notification::Event::PolicyTextsUpdated(updated) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                let mut settings = snapshot.settings.take().unwrap_or_default();
                settings.policy_texts = updated.texts;
                snapshot.settings = Some(settings).into();
            }
        }
        _ => {}
    }
}

fn merge_work_event(model: &mut Model, work: &w::WorkEvent) {
    model.companions.work_active = match &work.detail {
        Some(w::work_event::Detail::RunStarted(_)) => true,
        Some(w::work_event::Detail::StatusChanged(changed)) => {
            matches!(changed.status.as_known(), Some(w::WorkStatus::Active))
        }
        Some(w::work_event::Detail::RunFinished(finished)) => matches!(
            finished.state.as_known(),
            Some(w::RunStatus::Starting | w::RunStatus::Running | w::RunStatus::WaitingInput)
        ),
        None => false,
    };
}

fn merge_generic_notice(model: &mut Model, event: &w::HostEvent) {
    if event.kind != "notice" {
        return;
    }
    let Some(document) = event.payload.as_option() else {
        return;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&document.json) else {
        return;
    };
    let Some(message) = value.get("message").and_then(|item| item.as_str()) else {
        return;
    };
    let code = value
        .get("code")
        .and_then(|item| item.as_str())
        .unwrap_or_default();
    match code {
        "approval_resolved" => {
            let req_id = message.split(':').next().unwrap_or_default().trim();
            if !req_id.is_empty()
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                snapshot
                    .pending_approvals
                    .retain(|item| item.request_id != req_id);
            }
        }
        "approval_required" | "host_ready" => {}
        "model_unavailable" => {
            let has_enabled_llm = model.settings().is_some_and(|settings| {
                settings
                    .connections
                    .iter()
                    .any(|c| c.kind == w::ConnectionKind::Llm && c.enabled)
            });
            if has_enabled_llm {
                model.notice(NoticeKind::Failed, "ホストからの通知", message.to_owned());
            }
        }
        _ => {
            model.notice(NoticeKind::Failed, "ホストからの通知", message.to_owned());
        }
    }
}

/// Inserts or replaces one message, keeping the log ordered by the host's sequence.
pub fn merge_message(messages: &mut Vec<w::Message>, message: w::Message) {
    if let Some(existing) = messages
        .iter_mut()
        .find(|existing| existing.id == message.id)
    {
        *existing = message;
        return;
    }
    let at = messages.partition_point(|existing| existing.sequence <= message.sequence);
    messages.insert(at, message);
}

/// Keeps a pending approval current, and drops it once it is no longer pending.
pub fn upsert_approval(approvals: &mut Vec<w::ApprovalRequest>, request: w::ApprovalRequest) {
    let present = approvals
        .iter()
        .position(|approval| approval.request_id == request.request_id);
    if request.status == w::ApprovalStatus::Pending {
        match present {
            Some(index) => approvals[index] = request,
            None => approvals.push(request),
        }
    } else if let Some(index) = present {
        approvals.remove(index);
    }
}

fn upsert_by_revision<T>(
    items: &mut Vec<T>,
    item: T,
    key: impl Fn(&T) -> String,
    revision: impl Fn(&T) -> u64,
) {
    let id = key(&item);
    match items.iter().position(|existing| key(existing) == id) {
        Some(index) if revision(&items[index]) <= revision(&item) => items[index] = item,
        Some(_) => {}
        None => items.push(item),
    }
}

fn upsert_activation(activations: &mut Vec<w::Activation>, activation: w::Activation) {
    match activations
        .iter()
        .position(|current| current.companion_id == activation.companion_id)
    {
        Some(index) => activations[index] = activation,
        None => activations.push(activation),
    }
}
