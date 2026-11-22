//! ECS facade and plugin registration for [`super::Voice`].

use super::{AudioSubscription, InputBinding, Voice, companion_tts_source, parse_binding, speech};
use crate::host::{
    agent,
    db::{Result, Store},
    delegation,
    error::HostError,
    notify::Notifier,
    resources::HostRuntime,
    util,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

/// Binds an audio input stream to the client that opened it. Only the active
/// client of a companion may capture: a client that is not active has nothing
/// the head may answer.
pub(crate) fn bind_input(store: Option<&Store>, client_id: &str) -> Result<InputBinding> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(
            ("client_id is required for audio input").into(),
        ));
    }
    util::validate_id(client_id)?;
    let Some(store) = store else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client_repo().client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    if store
        .client_repo()
        .activation_for_client(client_id)?
        .is_none()
    {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    Ok(InputBinding {
        client_id: client_id.to_owned(),
    })
}

pub(crate) fn is_still_active(store: Option<&Store>, binding: &InputBinding) -> bool {
    let Some(store) = store else {
        return false;
    };
    store
        .client_repo()
        .activation_for_client(&binding.client_id)
        .is_ok_and(|activation| activation.is_some())
}

/// Binds an audio output stream to the client that opened it and makes it the
/// owner of every companion it is the active client of.
pub(crate) fn subscribe_audio(
    store: Option<&Store>,
    voice: Option<&Voice>,
    client_id: &str,
) -> Result<AudioSubscription> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(("client_id is required").into()));
    }
    util::validate_id(client_id)?;
    let Some(store) = store else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client_repo().client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let owned: Vec<String> = store
        .client_repo()
        .activations()?
        .into_iter()
        .filter(|activation| activation.client_id.as_deref() == Some(client_id))
        .map(|activation| activation.companion_id)
        .collect();
    if owned.is_empty() {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    let Some(voice) = voice else {
        return Err(HostError::Internal(
            ("the voice plugin is not installed").into(),
        ));
    };
    let subscription = voice.register(client_id);
    for companion_id in owned {
        voice.publish(&companion_id, client_id);
    }
    Ok(subscription)
}

pub(crate) fn unsubscribe_audio(voice: Option<&Voice>, client: &str, subscription: &str) {
    if let Some(voice) = voice {
        voice.unregister(client, subscription);
    }
}

/// Hands already-synthesized audio to the subscribed client as one utterance.
/// The speech floor is not taken, so a turn in flight keeps it.
pub(crate) fn play_once(
    voice: Option<&Voice>,
    companion: &str,
    pcm: Vec<u8>,
    rate: u32,
) -> std::result::Result<w::Empty, HostError> {
    let pcm = ene_voice::wire_pcm(pcm, rate, &mut None)
        .map_err(|error| HostError::Unavailable(error.to_string()))?;
    if pcm.is_empty() {
        return Err(HostError::Unavailable(
            ("the TTS engine returned no audio").into(),
        ));
    }
    let Some(voice) = voice else {
        return Err(HostError::Unavailable(
            ("the voice plugin is not installed").into(),
        ));
    };
    voice.play(companion, &pcm)?;
    Ok(w::Empty::default())
}

pub(crate) fn accept_transcription(
    world: &mut World,
    transcription_id: &str,
    text: &str,
    binding: &InputBinding,
) -> Result<bool> {
    let text = text.trim();
    if text.is_empty() || !is_still_active(world.get_resource::<Store>(), binding) {
        return Ok(false);
    }
    if let Some(voice) = world.get_resource::<Voice>()
        && !voice.accept_transcription(transcription_id)
    {
        return Ok(false);
    }
    delegation::record(world, transcription_id);
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(false);
    };
    let companions: Vec<String> = store
        .client_repo()
        .active_companions_for_client(&binding.client_id)?;
    let author = Author {
        request: Some(RequestId(transcription_id.to_owned())),
        ..Author::user()
    };
    for companion in companions {
        let Some(agent) = agent::agent_of(world, &companion) else {
            continue;
        };
        if let Some(mut conversation) = world.get_mut::<Conversation>(agent) {
            conversation.add_aside(Aside::new(
                "ene.transcription",
                Author::plugin("ene-host"),
                serde_json::json!({
                    "transcription_id": transcription_id,
                    "client_id": binding.client_id,
                }),
            ));
        }
        agent::deliver(
            world,
            agent,
            &companion,
            text.to_owned(),
            DeliveryMode::Steer,
            author.clone(),
        );
    }
    Ok(true)
}

pub(crate) fn on_stream_text(world: &mut World, companion: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return;
    };
    if !voice.owns(companion) {
        tracing::debug!(
            companion,
            "speech skipped: no client has subscribed to this companion's audio"
        );
        return;
    }
    if !voice.streaming(companion) && !try_begin_utterance(world, &voice, companion) {
        return;
    }
    voice.speak(companion, text);
}

fn try_begin_utterance(world: &mut World, voice: &Voice, companion: &str) -> bool {
    let companion_record = world
        .get_resource::<Store>()
        .and_then(|store| store.companion_repo().companion(companion).ok().flatten());
    let Ok(source) = companion_tts_source(world, companion_record.as_ref()) else {
        tracing::debug!(companion, "speech skipped: no TTS engine is configured");
        return false;
    };
    if voice
        .floor_holder()
        .is_some_and(|holder| holder != companion)
    {
        if let Some(agent) = agent::agent_of(world, companion) {
            world.trigger(StopTurn {
                entity: agent,
                reason: Some("speech floor was taken by a peer".to_owned()),
            });
        }
        return false;
    }
    let voice_name = companion_record
        .as_ref()
        .and_then(|record| parse_binding(record).voice_id)
        .unwrap_or_default();
    let opened = voice.begin(companion, source, voice_name);
    if !opened {
        tracing::debug!(companion, "speech skipped: the utterance was not opened");
    }
    opened
}

/// Ends the speaking turn and announces it, so a peer can pick up the
/// conversation without a new user input (voice-avatar.md §5.3).
pub(crate) fn on_turn_end(voice: Option<&Voice>, notifier: Option<&mut Notifier>, companion: &str) {
    let Some(voice) = voice else {
        return;
    };
    let Some(ended) = voice.end_turn(companion) else {
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
    let Some(notifier) = notifier else {
        return;
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

pub(crate) fn flush(voice: Option<&Voice>, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = voice else {
        return Err(HostError::NotFound(
            ("no client owns this companion's audio output").into(),
        ));
    };
    Ok(voice.flush(companion, reason)?)
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
    trigger: On<crate::host::resources::SpeakCharacterText>,
    mut commands: Commands,
) {
    let event = trigger.event();
    let companion_id = event.companion_id.clone();
    let text = event.text.clone();
    commands.queue(move |world: &mut World| {
        let spoken = speech::for_speech(&text);
        if !spoken.is_empty() {
            on_stream_text(world, &companion_id, &spoken);
            let voice = world.get_resource::<Voice>().cloned();
            let mut notifier = world.get_resource_mut::<Notifier>();
            on_turn_end(voice.as_ref(), notifier.as_deref_mut(), &companion_id);
        }
    });
}

pub(crate) struct VoicePlugin;

impl Plugin for VoicePlugin {
    fn build(&self, app: &mut App) {
        let runtime = app.world().resource::<HostRuntime>().0.clone();
        app.init_resource::<delegation::DeliveredClaims>()
            .insert_resource(Voice::new(runtime))
            .add_observer(observe_speak_character_text);
    }
}
