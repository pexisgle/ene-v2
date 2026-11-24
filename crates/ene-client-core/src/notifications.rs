//! Folding host notifications into the model, plus the merge primitives they use.
//!
//! The [`Update`](crate::update::Update) dispatch stays in `reducer`; this module
//! owns everything reached through [`merge_notification`].

use crate::{
    domain::{
        Activation, ApprovalRequest, ApprovalStatus, ConnectionKind, HostEvent, HostNotification,
        Message, NotificationEvent, RunStatus, TurnEvent, TurnStatus, WorkEvent, WorkEventDetail,
        WorkStatus,
    },
    reducer::is_user_facing_message,
    state::{Effects, Model, NoticeKind},
};

pub(crate) fn merge_notification(
    model: &mut Model,
    notification: HostNotification,
    effects: &mut Effects,
) {
    match notification.event {
        Some(
            event @ (NotificationEvent::InitialState(_)
            | NotificationEvent::Health(_)
            | NotificationEvent::Companion(_)
            | NotificationEvent::SettingsUpdated(_)
            | NotificationEvent::ClientUpdated(_)
            | NotificationEvent::ActivationUpdated(_)),
        ) => merge_snapshot_event(model, event, effects),
        Some(
            event @ (NotificationEvent::MessageCreated(_)
            | NotificationEvent::MessageDelta(_)
            | NotificationEvent::TurnEvent(_)
            | NotificationEvent::ResyncRequired { .. }),
        ) => merge_chat_event(model, event),
        Some(
            event @ (NotificationEvent::ApprovalRequested(_)
            | NotificationEvent::PolicyRulesUpdated(_)
            | NotificationEvent::PolicyTextsUpdated(_)),
        ) => merge_approval_event(model, event, effects),
        Some(NotificationEvent::WorkEvent(work)) => merge_work_event(model, &work),
        Some(NotificationEvent::Generic(event)) => merge_generic_notice(model, &event),
        Some(NotificationEvent::MindUpdated(updated)) => {
            effects.mind |= model.companion_id() == Some(updated.companion_id.as_str())
                || (!updated.companion_id.is_empty()
                    && model.companions.chat.companion_id == updated.companion_id);
        }
        Some(NotificationEvent::Other) | None => {}
    }
}

fn merge_snapshot_event(model: &mut Model, event: NotificationEvent, effects: &mut Effects) {
    match event {
        NotificationEvent::InitialState(mut snapshot) => {
            snapshot
                .companions
                .sort_by(|left, right| left.name.cmp(&right.name));
            model.companions.work_active = snapshot
                .work_sessions
                .iter()
                .any(|session| session.status == WorkStatus::Active);
            model.snapshot = Some(*snapshot);
            effects.approvals = false;
        }
        NotificationEvent::Health(health) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.health = Some(health);
            }
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
        _ => {}
    }
}

fn merge_chat_event(model: &mut Model, event: NotificationEvent) {
    let companions = &mut model.companions;
    match event {
        NotificationEvent::MessageCreated(message) => {
            if message.companion_id == companions.chat.companion_id
                && is_user_facing_message(&message)
            {
                companions
                    .streaming
                    .retain(|(companion, _), _| companion != &message.companion_id);
                merge_message(&mut companions.chat.messages, *message);
            }
        }
        NotificationEvent::MessageDelta(delta) => {
            if delta.companion_id == companions.chat.companion_id {
                let text = companions
                    .streaming
                    .entry((delta.companion_id, delta.stream_id))
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
    if turn.companion_id != companions.chat.companion_id {
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

fn merge_approval_event(model: &mut Model, event: NotificationEvent, effects: &mut Effects) {
    match event {
        NotificationEvent::ApprovalRequested(requested) => {
            if let Some(request) = requested
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_approval(&mut snapshot.pending_approvals, *request);
            } else {
                effects.approvals = true;
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
        _ => {}
    }
}

fn merge_work_event(model: &mut Model, work: &WorkEvent) {
    model.companions.work_active = match &work.detail {
        Some(WorkEventDetail::RunStarted) => true,
        Some(WorkEventDetail::StatusChanged(status)) => *status == WorkStatus::Active,
        Some(WorkEventDetail::RunFinished { state, .. }) => matches!(
            state,
            RunStatus::Starting | RunStatus::Running | RunStatus::WaitingInput
        ),
        None => false,
    };
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
    if request.status == ApprovalStatus::Pending {
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
