//! Folding host notifications into the model, plus the merge primitives they use.
//!
//! The [`Update`](crate::update::Update) dispatch stays in `reducer`; this module
//! owns everything reached through [`merge_notification`].

use crate::{
    domain::{
        Activation, ApprovalRequest, ApprovalStatus, ConnectionKind, HostCursor, HostEvent,
        HostNotification, Message, NotificationEvent, TurnEvent, TurnStatus, WorkStatus,
    },
    reducer::is_user_facing_message,
    state::{Chat, Effects, Model, NoticeKind},
};

/// The notification stream may lag behind an already received unary snapshot.
#[derive(Debug, Default)]
pub(crate) struct HostOrder {
    epoch: Option<String>,
    stream_seq: Option<u64>,
    snapshot_seq: Option<u64>,
}

impl HostOrder {
    pub(crate) fn accept(&mut self, notification: &HostNotification) -> bool {
        let is_snapshot = matches!(notification.event, Some(NotificationEvent::InitialState(_)));
        let Some(cursor) = &notification.cursor else {
            return !is_snapshot || self.epoch.is_none();
        };
        let epoch = self.epoch.get_or_insert_with(|| cursor.epoch.clone());
        if *epoch != cursor.epoch {
            return false;
        }
        let seq = Some(cursor.seq);
        if is_snapshot {
            if seq < self.stream_seq.max(self.snapshot_seq) {
                return false;
            }
            self.snapshot_seq = seq;
            return true;
        }
        if seq <= self.stream_seq {
            return false;
        }
        self.stream_seq = seq;
        // A snapshot includes current host state but no chat or mind events.
        seq > self.snapshot_seq || !is_snapshot_state(notification.event.as_ref())
    }
}

fn is_snapshot_state(event: Option<&NotificationEvent>) -> bool {
    matches!(
        event,
        Some(
            NotificationEvent::Companion(_)
                | NotificationEvent::CompanionRemoved(_)
                | NotificationEvent::SettingsUpdated(_)
                | NotificationEvent::ClientUpdated(_)
                | NotificationEvent::ActivationUpdated(_)
                | NotificationEvent::WorkStatusChanged { .. }
                | NotificationEvent::ApprovalRequested(_)
                | NotificationEvent::PolicyRulesUpdated(_)
                | NotificationEvent::PolicyTextsUpdated(_)
        )
    )
}

pub(crate) fn merge_notification(
    model: &mut Model,
    notification: HostNotification,
    effects: &mut Effects,
) {
    let Some(event) = notification.event else {
        return;
    };
    match event {
        NotificationEvent::CompanionRemoved(id) => {
            remove_companion(model, id.as_str(), effects);
        }
        NotificationEvent::InitialState(mut snapshot) => {
            snapshot
                .companions
                .sort_by(|left, right| left.name.cmp(&right.name));
            model.snapshot = Some(*snapshot);
        }
        NotificationEvent::Companion(companion) => {
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
        NotificationEvent::SettingsUpdated(Some(settings)) => {
            if let Some(snapshot) = model.snapshot.as_mut()
                && snapshot
                    .settings
                    .as_ref()
                    .is_none_or(|current| current.revision <= settings.revision)
            {
                snapshot.settings = Some(*settings);
            }
        }
        NotificationEvent::ClientUpdated(Some(client)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_by_revision(
                    &mut snapshot.clients,
                    *client,
                    |item| item.id.clone(),
                    |item| item.revision,
                );
            }
        }
        NotificationEvent::ActivationUpdated(activation) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_activation(&mut snapshot.activations, activation);
            }
        }
        event @ (NotificationEvent::MessageCreated(_)
        | NotificationEvent::MessageDelta(_)
        | NotificationEvent::TurnEvent(_)
        | NotificationEvent::ResyncRequired { .. }) => {
            merge_chat_event(model, event, notification.cursor)
        }
        NotificationEvent::ApprovalRequested(requested) => {
            if let Some(request) = requested
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_approval(&mut snapshot.pending_approvals, *request);
            } else {
                effects.snapshot = true;
            }
        }
        NotificationEvent::PolicyRulesUpdated(rules) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.policy_rules = rules;
            }
        }
        NotificationEvent::PolicyTextsUpdated(texts) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                let settings = snapshot.settings.get_or_insert_with(Default::default);
                settings.policy_texts = texts;
            }
        }
        NotificationEvent::WorkStatusChanged {
            work_id,
            companion_id,
            status,
        } => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                if status == WorkStatus::Active {
                    snapshot.active_work.insert(work_id, companion_id);
                } else {
                    snapshot.active_work.remove(&work_id);
                }
            }
        }
        NotificationEvent::Generic(event) => merge_generic_notice(model, &event),
        NotificationEvent::MindUpdated(companion) => {
            effects.mind |= model.companion_id() == Some(companion.as_str())
                || model.companions.chat.companion_id.as_ref() == Some(&companion);
        }
        NotificationEvent::SettingsUpdated(None)
        | NotificationEvent::ClientUpdated(None)
        | NotificationEvent::Other => {}
    }
}

pub(crate) fn remove_companion(model: &mut Model, companion_id: &str, effects: &mut Effects) {
    let was_shown = model.companion_id() == Some(companion_id);
    if let Some(snapshot) = model.snapshot.as_mut() {
        snapshot
            .companions
            .retain(|companion| companion.id != companion_id);
        snapshot
            .activations
            .retain(|activation| activation.companion_id.as_str() != companion_id);
        if snapshot.active_companion_id.as_deref() == Some(companion_id) {
            snapshot.active_companion_id = snapshot
                .activations
                .iter()
                .find(|activation| activation.client_id.is_some())
                .map(|activation| activation.companion_id.to_string());
        }
        snapshot
            .active_work
            .retain(|_, owner| owner.as_str() != companion_id);
    }
    model.companions.minds.remove(companion_id);
    model
        .companions
        .streaming
        .retain(|(id, _), _| id != companion_id);
    if model.companions.selected.as_deref() == Some(companion_id) {
        model.companions.selected = None;
    }
    if model
        .companions
        .chat
        .companion_id
        .as_ref()
        .map(ene_domain::CompanionId::as_str)
        == Some(companion_id)
    {
        model.companions.chat = Chat::default();
    }
    if was_shown && model.companion_id().is_some() {
        effects.history = true;
        effects.mind = true;
    }
}

fn merge_chat_event(model: &mut Model, event: NotificationEvent, cursor: Option<HostCursor>) {
    let companions = &mut model.companions;
    match event {
        NotificationEvent::MessageCreated(message) => {
            if message.companion_id.is_some()
                && companions.chat.companion_id == message.companion_id
                && is_user_facing_message(&message)
            {
                companions.streaming.retain(|(companion, _), _| {
                    message
                        .companion_id
                        .as_ref()
                        .is_none_or(|id| companion != id.as_str())
                });
                if cursor.is_some() {
                    companions.chat.latest_live_message = cursor;
                }
                merge_message(&mut companions.chat.messages, *message);
            }
        }
        NotificationEvent::MessageDelta(delta) => {
            if companions
                .chat
                .companion_id
                .as_ref()
                .is_some_and(|id| id.as_str() == delta.companion_id)
            {
                let text = companions
                    .streaming
                    .entry((delta.companion_id.as_str().to_owned(), delta.stream_id))
                    .or_default();
                text.push_str(&delta.delta);
            }
        }
        NotificationEvent::TurnEvent(turn) => merge_turn_event(model, *turn),
        NotificationEvent::ResyncRequired { .. } => companions.streaming.clear(),
        _ => {}
    }
}

fn merge_turn_event(model: &mut Model, turn: TurnEvent) {
    let companions = &mut model.companions;
    if turn.companion_id.is_none() || companions.chat.companion_id != turn.companion_id {
        return;
    }
    let terminal = matches!(
        turn.status,
        TurnStatus::Answered | TurnStatus::Stopped | TurnStatus::Failed
    );
    if terminal {
        companions.streaming.clear();
        companions.chat.stop_requested = false;
    }
    let failed_message = if turn.status == TurnStatus::Failed {
        turn.failure
            .as_ref()
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

fn merge_generic_notice(model: &mut Model, event: &HostEvent) {
    if event.kind != "notice" {
        return;
    }
    let Some(document) = event.payload.as_ref() else {
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
                    .any(|c| c.kind == ConnectionKind::Llm && c.enabled)
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
pub fn merge_message(messages: &mut Vec<Message>, message: Message) {
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
pub fn upsert_approval(approvals: &mut Vec<ApprovalRequest>, request: ApprovalRequest) {
    let present = approvals
        .iter()
        .position(|approval| approval.request_id == request.request_id);
    if request.status == Some(ApprovalStatus::Pending) {
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

fn upsert_activation(activations: &mut Vec<Activation>, activation: Activation) {
    match activations
        .iter()
        .position(|current| current.companion_id == activation.companion_id)
    {
        Some(index) => activations[index] = activation,
        None => activations.push(activation),
    }
}
