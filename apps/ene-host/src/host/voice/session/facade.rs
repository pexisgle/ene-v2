use super::{Voice, parse_binding, synthesize};
use crate::host::voice::{
    HttpResult, MAX_PROBE_CHARS, companion_tts_source, engine::TtsEngine, pcm::wire_pcm, speech,
};
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
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub(crate) struct InputBinding {
    pub(crate) client_id: String,
}

impl InputBinding {
    pub(crate) fn accepts(&self, client_id: &str) -> bool {
        client_id.is_empty() || client_id == self.client_id
    }
}

/// Binds an audio input stream to the client that opened it. Only the active
/// client of a companion may capture: a client that is not active has nothing
/// the head may answer.
pub(crate) fn bind_input(world: &World, client_id: &str) -> Result<InputBinding> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(
            ("client_id is required for audio input").into(),
        ));
    }
    util::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>() else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    if store.activation_for_client(client_id)?.is_none() {
        return Err(HostError::Conflict(
            ("the client is not the active client of any companion").into(),
        ));
    }
    Ok(InputBinding {
        client_id: client_id.to_owned(),
    })
}

pub(crate) fn is_still_active(world: &World, binding: &InputBinding) -> bool {
    let Some(store) = world.get_resource::<Store>() else {
        return false;
    };
    store
        .activation_for_client(&binding.client_id)
        .is_ok_and(|activation| activation.is_some())
}

pub(crate) struct AudioSubscription {
    pub(crate) subscription_id: String,
    pub(crate) receiver: mpsc::Receiver<w::AudioOutputEvent>,
    pub(crate) control: mpsc::UnboundedReceiver<w::AudioOutputEvent>,
}

/// Binds an audio output stream to the client that opened it and makes it the
/// owner of every companion it is the active client of.
pub(crate) fn subscribe_audio(world: &mut World, client_id: &str) -> Result<AudioSubscription> {
    if client_id.is_empty() {
        return Err(HostError::Invalid(("client_id is required").into()));
    }
    util::validate_id(client_id)?;
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Err(HostError::Unavailable(
            "the host state is not available".into(),
        ));
    };
    if store.client(client_id)?.is_none() {
        return Err(HostError::NotFound(("client not found").into()));
    }
    let owned: Vec<String> = store
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
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
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

pub(crate) fn unsubscribe_audio(world: &World, client: &str, subscription: &str) {
    if let Some(voice) = world.get_resource::<Voice>() {
        voice.unregister(client, subscription);
    }
}

/// Speaks `text` through `companion`'s audio client, outside any turn. The
/// speech floor is not taken and no conversation is touched: this is the probe a
/// settings screen uses to hear the TTS connection, so it must not disturb the
/// turn the person is in the middle of.
pub(crate) async fn synthesize_for_probe(
    source: Arc<dyn TtsEngine>,
    text: &str,
    voice: String,
) -> HttpResult<(Vec<u8>, u32)> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(crate::host::voice::VoiceHttpError::TextRequired);
    }
    if text.chars().count() > MAX_PROBE_CHARS {
        return Err(crate::host::voice::VoiceHttpError::TextTooLong(
            MAX_PROBE_CHARS,
        ));
    }
    synthesize(&*source, text, voice).await
}

/// Hands already-synthesized audio to the subscribed client as one utterance.
/// The speech floor is not taken, so a turn in flight keeps it.
pub(crate) fn play_once(
    world: &World,
    companion: &str,
    pcm: Vec<u8>,
    rate: u32,
) -> std::result::Result<w::Empty, HostError> {
    let pcm = wire_pcm(pcm, rate, &mut None)
        .map_err(|error| HostError::Unavailable(error.to_string()))?;
    if pcm.is_empty() {
        return Err(HostError::Unavailable(
            ("the TTS engine returned no audio").into(),
        ));
    }
    let Some(voice) = world.get_resource::<Voice>() else {
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
    if text.is_empty() || !is_still_active(world, binding) {
        return Ok(false);
    }
    if let Some(voice) = world.get_resource::<Voice>()
        && !voice.accept_transcription(transcription_id)
    {
        return Ok(false);
    }
    // The delegation claim of `start_work` is keyed by this transcription id.
    delegation::record(world, transcription_id);
    let Some(store) = world.get_resource::<Store>().cloned() else {
        return Ok(false);
    };
    let companions: Vec<String> = store.active_companions_for_client(&binding.client_id)?;
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
        .and_then(|store| store.companion(companion).ok().flatten());
    let Ok(source) = companion_tts_source(world, companion_record.as_ref()) else {
        tracing::debug!(companion, "speech skipped: no TTS engine is configured");
        return false;
    };
    if preempt_by_peer_floor(world, voice, companion) {
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

fn preempt_by_peer_floor(world: &mut World, voice: &Voice, companion: &str) -> bool {
    let Some(holder) = voice.floor_holder() else {
        return false;
    };
    if holder == companion {
        return false;
    }
    if let Some(agent) = agent::agent_of(world, companion) {
        world.trigger(StopTurn {
            entity: agent,
            reason: Some("speech floor was taken by a peer".to_owned()),
        });
    }
    true
}

/// Ends the speaking turn and announces it, so a peer can pick up the
/// conversation without a new user input (voice-avatar.md §5.3).
pub(crate) fn on_turn_end(world: &mut World, companion: &str) {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
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
    let Some(mut notifier) = world.get_resource_mut::<Notifier>() else {
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

pub(crate) fn flush(world: &mut World, companion: &str, reason: w::FlushReason) -> Result<()> {
    let Some(voice) = world.get_resource::<Voice>().cloned() else {
        return Err(HostError::NotFound(
            ("no client owns this companion's audio output").into(),
        ));
    };
    voice.flush(companion, reason)
}

pub(crate) fn on_client_moved(world: &mut World, companion: &str, client: &str) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        voice.client_moved(companion, client);
    }
}

pub(crate) fn on_client_activated(world: &mut World, companion: &str, client: &str) {
    if let Some(voice) = world.get_resource::<Voice>().cloned() {
        let mut state = voice.lock();
        if state.streams.contains_key(client) {
            let owner = state.owners.entry(companion.to_owned()).or_default();
            owner.client_id = client.to_owned();
        }
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
            on_turn_end(world, &companion_id);
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
