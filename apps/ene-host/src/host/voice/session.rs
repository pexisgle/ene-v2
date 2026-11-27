//! ECS facade and plugin registration for [`super::Voice`].

use super::{AudioSubscription, Voice, companion_tts_source, parse_binding, speech};
use crate::host::{
    agent,
    db::{Result, Store},
    error::HostError,
    notify::Notifier,
};
use bevy_ecs::prelude::*;
use ene_domain::ClientId;
use ene_rpc as w;
use rig_ecs::prelude::*;

/// Resolves the companions an active client can speak to or hear.
pub(crate) fn active_companions(store: &Store, client: &ClientId) -> Result<Vec<String>> {
    let repo = store.client_repo();
    if repo.client(client.as_str())?.is_none() {
        return Err(HostError::NotFound("client not found".into()));
    }
    let owned = repo.active_companions_for_client(client.as_str())?;
    if owned.is_empty() {
        return Err(HostError::Conflict(
            "the client is not the active client of any companion".into(),
        ));
    }
    Ok(owned)
}

/// Binds an audio output stream to the client that opened it.
pub(crate) fn subscribe_audio(
    store: &Store,
    voice: &Voice,
    client: &ClientId,
) -> Result<AudioSubscription> {
    let companions = active_companions(store, client)?;
    let subscription = voice.register(client.as_str());
    for companion in companions {
        voice.publish(&companion, client.as_str());
    }
    Ok(subscription)
}

/// Hands already-synthesized audio to the subscribed client as one utterance.
/// The speech floor is not taken, so a turn in flight keeps it.
pub(crate) fn play_once(
    voice: &Voice,
    companion: &str,
    client: &str,
    subscription: &str,
    pcm: Vec<u8>,
    rate: u32,
) -> Result<()> {
    let pcm = ene_voice::wire_pcm(pcm, rate, &mut None)
        .map_err(|error| HostError::Unavailable(error.to_string()))?;
    if pcm.is_empty() {
        return Err(HostError::Unavailable(
            ("the TTS engine returned no audio").into(),
        ));
    }
    Ok(voice.play(companion, client, subscription, &pcm)?)
}

pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    let Some(voice) = world.get_resource::<Voice>() else {
        return;
    };
    if text.is_empty() || !voice.owns(companion) {
        return;
    }
    if voice.streaming(companion) {
        voice.speak(companion, text);
        return;
    }
    let context = super::VoiceEngineContext {
        store: world.get_resource::<Store>(),
        credentials: world.get_resource::<crate::host::model::Credentials>(),
        providers: world.get_resource::<crate::host::providers::Providers>(),
        engines: world
            .get_resource::<super::Engines>()
            .cloned()
            .unwrap_or_default(),
    };
    if let SpeechEffect::StopTurn = speak_new_utterance(&context, voice, companion, text)
        && let Some(agent) = agent::agent_of(world, companion)
    {
        world.trigger(StopTurn::plugin_alone(
            agent,
            "ene-host",
            "speech floor was taken by a peer",
        ));
    }
}

enum SpeechEffect {
    ContinueTurn,
    StopTurn,
}

fn speak_new_utterance(
    context: &super::VoiceEngineContext<'_>,
    voice: &Voice,
    companion: &str,
    text: &str,
) -> SpeechEffect {
    let record = context
        .store
        .and_then(|store| store.companion_repo().companion(companion).ok().flatten());
    let Ok(source) = companion_tts_source(context, record.as_ref()) else {
        return SpeechEffect::ContinueTurn;
    };
    if voice
        .floor_holder()
        .is_some_and(|holder| holder != companion)
    {
        return SpeechEffect::StopTurn;
    }
    let voice_name = record
        .as_ref()
        .and_then(|record| parse_binding(record).voice_id)
        .unwrap_or_default();
    if voice.begin(companion, source, voice_name) {
        voice.speak(companion, text);
    }
    SpeechEffect::ContinueTurn
}

/// Ends the speaking turn and announces it, so a peer can pick up the
/// conversation without a new user input (voice-avatar.md §5.3).
pub(crate) fn on_turn_end(voice: Option<&Voice>, notifier: Option<&Notifier>, companion: &str) {
    let (Some(ended), Some(notifier)) =
        (voice.and_then(|voice| voice.end_turn(companion)), notifier)
    else {
        return;
    };
    let document = w::JsonDocument {
        schema: "ene.speech_completed.v1".into(),
        json: serde_json::json!({
            "companion_id": companion,
            "client_id": ended.client_id,
            "utterance_id": ended.utterance_id,
            "generation_id": ended.generation_id,
            "total_sentences": ended.total_sentences,
        })
        .to_string(),
        ..Default::default()
    };
    notifier.push(
        Some(companion),
        None,
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: "speech_completed".into(),
            payload: Some(document).into(),
            ..Default::default()
        })),
    );
}

/// Wire flush reason -> voice domain reason.
fn flush_reason(reason: w::FlushReason) -> ene_voice::FlushReason {
    match reason {
        w::FlushReason::BargeIn => ene_voice::FlushReason::BargeIn,
        w::FlushReason::Cancelled => ene_voice::FlushReason::Cancelled,
        w::FlushReason::ClientMoved => ene_voice::FlushReason::ClientMoved,
        w::FlushReason::Unspecified => ene_voice::FlushReason::Unspecified,
    }
}

pub(crate) fn flush(voice: Option<&Voice>, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = voice else {
        return Err(HostError::NotFound(
            ("no client owns this companion's audio output").into(),
        ));
    };
    Ok(voice.flush(companion, flush_reason(reason))?)
}

pub(crate) fn on_client_moved(voice: Option<&Voice>, companion: &str, client: &str) {
    if let Some(voice) = voice {
        voice.client_moved(companion, client);
    }
}

pub(crate) fn on_client_activated(voice: Option<&Voice>, companion: &str, client: &str) {
    if let Some(voice) = voice {
        voice.activate_client_if_subscribed(companion, client);
    }
}

fn observe_speak_character_text(
    event: &crate::host::resources::SpeakCharacterText,
    mut commands: Commands,
) {
    let spoken = speech::for_speech(&event.text);
    if spoken.is_empty() {
        return;
    }
    let companion = event.companion_id.clone();
    commands.queue(move |world: &mut World| {
        on_stream_text(world, &companion, &spoken);
        on_turn_end(
            world.get_resource::<Voice>(),
            world.get_resource::<Notifier>(),
            &companion,
        );
    });
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(
            |trigger: On<crate::host::resources::SpeakCharacterText>, commands: Commands| {
                observe_speak_character_text(trigger.event(), commands);
            },
        );
    }
}
