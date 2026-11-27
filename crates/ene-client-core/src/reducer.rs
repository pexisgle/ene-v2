//! Pure state reducer folding [`Update`] messages and host notifications into [`Model`].

use crate::{
    domain::{Message, MessageRole},
    notifications,
    state::{
        AudioState, Chat, Effects, Link, LiveTranscription, Model, NoticeKind, PerceptionState,
        SpeechReport, Unsent,
    },
    update::Update,
};

// The merge primitives moved to `notifications` with the code that uses them;
// re-exported here so `reducer::merge_message` and `reducer::upsert_approval`
// stay the public paths callers already import.
pub use crate::notifications::{merge_message, upsert_approval};

impl Model {
    /// Admits a reply before any reducer or front-end effects use it.
    pub fn accept_update(&mut self, mut update: Update) -> Option<Update> {
        while let Update::Scoped { epoch, seq, inner } = update {
            if epoch != self.connections.link_epoch {
                return None;
            }
            if let Some(target) = inner.reply_target() {
                let applied = self.connections.applied_seq.entry(target).or_default();
                if seq <= *applied {
                    return None;
                }
                *applied = seq;
            }
            update = *inner;
        }
        Some(update)
    }

    /// Folds one update into the view and reports what must be refetched.
    pub fn apply(&mut self, update: Update) -> Effects {
        let mut effects = Effects::default();
        let Some(update) = self.accept_update(update) else {
            return effects;
        };
        match update {
            Update::Link(link) => apply_link(self, *link, &mut effects),
            Update::Notification(notification) => {
                notifications::merge_notification(self, *notification, &mut effects);
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
                result,
                ..
            } => match result {
                Ok(check) => {
                    self.connections.checks.insert(connection_id, check);
                }
                Err(notice) => self.notice(notice.kind, notice.context, notice.message),
            },
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
        model.connections.applied_seq.clear();
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
        | Update::MindLoaded { .. }
        | Update::CompanionTemplates(_)
        | Update::CompanionExported { .. }
        | Update::CompanionPreview { .. } => apply_companion_update(model, update),
        Update::Gone { companion_id } => {
            notifications::remove_companion(model, &companion_id, effects);
        }
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
            if companions
                .chat
                .companion_id
                .as_ref()
                .is_some_and(|id| id.as_str() == companion_id)
            {
                companions.chat.loading = false;
                companions.chat.error = Some(message);
            }
        }
        Update::StopRequested { companion_id } => {
            if companions
                .chat
                .companion_id
                .as_ref()
                .is_some_and(|id| id.as_str() == companion_id)
            {
                companions.chat.stop_requested = true;
            }
        }
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

fn apply_history(chat: &mut Chat, companion_id: &str, messages: Vec<Message>) {
    if chat
        .companion_id
        .as_ref()
        .is_none_or(|id| id.as_str() != companion_id)
    {
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
            .any(|message| message.role == MessageRole::User && message.text == sending.text)
    });
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
                let settings = snapshot.settings.get_or_insert_with(Default::default);
                settings.policy_texts = texts;
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
            // Keep the newest 20 lines: `split_off` returns the tail, so it
            // must be the retained half, never the discarded one.
            let overflow = audio.recognized.len().saturating_sub(20);
            audio.recognized.drain(..overflow);
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
pub fn is_user_facing_message(message: &Message) -> bool {
    if !message.work_id.trim().is_empty() {
        return false;
    }
    if matches!(message.role, MessageRole::Agent | MessageRole::Context) {
        return false;
    }
    let trimmed = message.text.trim();
    !(trimmed.starts_with("[Output of ")
        || trimmed.starts_with("[作業報告")
        || trimmed.starts_with("[作業結果")
        || trimmed.starts_with("[hand ")
        || trimmed.starts_with("The hand finished"))
}

#[cfg(test)]
mod reducer_property_tests {
    use super::*;
    use proptest::prelude::*;

    fn any_id() -> impl Strategy<Value = String> {
        "[a-z0-9]{1,8}".prop_map(|s| s)
    }

    fn any_message() -> impl Strategy<Value = Message> {
        (any_id(), "[ -~]{0,32}", any::<u64>()).prop_map(|(id, text, sequence)| Message {
            id,
            companion_id: Some(ene_domain::CompanionId::parse("ene").unwrap()),
            work_id: String::new(),
            role: crate::domain::MessageRole::default(),
            text,
            created_at: String::new(),
            sequence,
            request_id: None,
            author: String::new(),
        })
    }

    proptest! {
        /// A message merged twice appears once. Notifications can be replayed
        /// after a resubscribe, so duplicates in the list would be visible.
        #[test]
        fn merging_a_message_twice_yields_one_row(message in any_message()) {
            let mut messages = Vec::new();
            merge_message(&mut messages, message.clone());
            merge_message(&mut messages, message);
            prop_assert_eq!(messages.len(), 1);
        }

        /// Merging a sequence never loses an id: every distinct id that was
        /// ever merged is present exactly once at the end.
        #[test]
        fn merging_keeps_each_id_exactly_once(batch in prop::collection::vec(any_message(), 0..12)) {
            let mut messages = Vec::new();
            for message in &batch {
                merge_message(&mut messages, message.clone());
            }
            let mut ids: Vec<&String> = messages.iter().map(|m| &m.id).collect();
            let total = ids.len();
            ids.sort();
            ids.dedup();
            prop_assert_eq!(ids.len(), total, "an id appeared twice");

            for message in &batch {
                prop_assert!(
                    messages.iter().any(|m| m.id == message.id),
                    "{} vanished",
                    message.id
                );
            }
        }

        /// Merging is order-independent for the final id set: the same messages
        /// in a different order produce the same set, so a reordered replay
        /// cannot change what the window shows.
        #[test]
        fn the_resulting_id_set_does_not_depend_on_order(
            batch in prop::collection::vec(any_message(), 0..8),
        ) {
            let mut forward = Vec::new();
            for message in &batch {
                merge_message(&mut forward, message.clone());
            }
            let mut backward = Vec::new();
            for message in batch.iter().rev() {
                merge_message(&mut backward, message.clone());
            }
            let mut a: Vec<&String> = forward.iter().map(|m| &m.id).collect();
            let mut b: Vec<&String> = backward.iter().map(|m| &m.id).collect();
            a.sort();
            b.sort();
            prop_assert_eq!(a, b);
        }
    }
}
