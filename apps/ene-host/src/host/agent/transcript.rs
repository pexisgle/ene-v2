//! Projecting a conversation into the notification stream.
//!
//! [`entry_text`] and [`role_of`] decide what one conversation entry means,
//! and the systems here publish it: transcript messages, and the text deltas of
//! an ordinary companion turn.

use super::companion_of;
use crate::host::{
    notify::Notifier,
    resources::{CompanionAgent, LastProjectedEntry, WorkAgent},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::completion::Message as CompletionMessage;
use rig_core::conversation::{AuthorKind, EntryRef};
use rig_core::message::UserContent;
use rig_ecs::prelude::*;

/// The conversation text of one entry, when it belongs in the transcript.
pub(crate) fn entry_text(entry: &EntryRef<'_>) -> Option<String> {
    if entry.author.is_some_and(crate::host::eyes::is_eyes) {
        return None;
    }
    let text: String = match entry.message {
        CompletionMessage::User { content } => {
            content.iter().filter_map(UserContent::as_text).collect()
        }
        CompletionMessage::Assistant(reply) => reply.text(),
        CompletionMessage::System { .. } => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

pub(crate) fn role_of(entry: &EntryRef<'_>) -> w::MessageRole {
    match (entry.author.map(|author| &author.kind), entry.message) {
        (None, CompletionMessage::User { .. }) => w::MessageRole::User,
        (None, CompletionMessage::System { .. }) => w::MessageRole::Context,
        (None, CompletionMessage::Assistant(_)) => w::MessageRole::Assistant,
        (Some(AuthorKind::User), _) => w::MessageRole::User,
        (Some(AuthorKind::Agent), _) => w::MessageRole::Agent,
        (Some(AuthorKind::Plugin(_)), _) | (Some(AuthorKind::Rig), _) => w::MessageRole::Context,
    }
}

/// The agent rows a message projection needs.
type AgentRows = (
    &'static AgentId,
    &'static Conversation,
    Option<&'static CompanionAgent>,
    Option<&'static WorkAgent>,
    &'static mut LastProjectedEntry,
);

type UnpublishedMessages = Or<(Changed<Conversation>, Added<LastProjectedEntry>)>;

/// Projects every new conversation entry into `MessageCreated` notifications,
/// and speaks the entries that are a companion's own reply.
pub(crate) fn publish_messages(
    mut agents: Query<AgentRows, UnpublishedMessages>,
    notifier: Res<Notifier>,
    mut commands: Commands,
) {
    let notifier = notifier.into_inner();
    for (_, conversation, companion, work, mut published) in &mut agents {
        let Some(companion_id) = companion_of(companion, work) else {
            continue;
        };
        let work_id = work.map_or_default(|agent| agent.work_id.as_str());
        let start = conversation
            .entries()
            .rposition(|entry| published.0.is_some_and(|last| entry.id.get() <= last))
            .map_or(0, |at| at + 1);
        for entry in conversation.entries().skip(start) {
            published.0 = Some(entry.id.get());
            let Some(message) = crate::host::chat::message(companion_id, work_id, &entry) else {
                continue;
            };
            // Only a companion's own reply is spoken, and only to the user. A
            // whole message is what a sentence-level engine like VOICEVOX wants:
            // the streaming fragments are too small to split into sentences, and
            // a fragment can arrive after its call settled and be dropped.
            if companion.is_some() && message.role == w::MessageRole::Assistant {
                let voice_companion = companion_id.to_owned();
                let voice_text = crate::host::voice::speech::for_speech(&message.text);
                let finished_text = message.text.clone();
                commands.queue(move |world: &mut World| {
                    if !voice_text.is_empty() {
                        crate::host::voice::on_stream_text(world, &voice_companion, &voice_text);
                    }
                    // The reply is complete, so this turn's utterance is too.
                    // Without this the host never sends `UtteranceFinished`, the
                    // window never reports playback, and the speech floor stays
                    // held until the next turn flushes it as cancelled.
                    crate::host::voice::on_turn_end(
                        world.get_resource::<crate::host::voice::Voice>(),
                        world.get_resource::<Notifier>(),
                        &voice_companion,
                    );
                    world.trigger(crate::host::resources::TriggerCharacterEvent {
                        companion_id: voice_companion,
                        event: ene_plugin::CharacterEvent::ConversationFinished {
                            text: finished_text,
                        },
                    });
                });
            }
            notifier.push(
                Some(companion_id),
                None,
                w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                    message: Some(message).into(),
                    ..Default::default()
                })),
            );
        }
    }
}

/// Reports text deltas of ordinary conversation turns only: never reasoning,
/// summaries or child-agent output.
///
/// These fragments feed the window's live preview, not speech: a partial can
/// arrive after its call settled and be dropped, and a fragment is too small to
/// split into sentences. Speech is driven by the recorded reply instead.
pub(super) fn stream_deltas(
    mut deltas: MessageReader<StreamDelta>,
    calls: Query<(), (With<TurnCall>, Without<Settled>)>,
    agents: Query<&CompanionAgent, With<AgentId>>,
    notifier: Res<Notifier>,
) {
    let notifier = notifier.into_inner();
    for delta in deltas.read() {
        if !calls.contains(delta.effect) {
            continue;
        }
        let StreamEvent::Text { text, .. } = &delta.event else {
            continue;
        };
        // Only a companion speaks to the user; a work agent's text is a report
        // to the head, never a transcript delta of its own.
        let Some(agent) = delta.agent.and_then(|agent| agents.get(agent).ok()) else {
            continue;
        };
        notifier.push(
            Some(agent.companion_id.as_str()),
            None,
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                companion_id: agent.companion_id.clone(),
                stream_id: delta.effect.to_string(),
                delta: text.clone(),
                ..Default::default()
            })),
        );
    }
}
