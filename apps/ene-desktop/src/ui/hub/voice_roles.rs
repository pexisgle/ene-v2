//! Model and voice selection, STT/TTS assignment, and companion voice sync for [`Hub`].

use super::Hub;
use crate::rpc;
use ene_client_core::connections::{build_stt_assignment_inputs, build_tts_assignment_inputs};
use gpui::Context;

pub(crate) fn assign_stt_model(
    hub: &mut Hub,
    selection: Option<(&str, &str)>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    let inputs = build_stt_assignment_inputs(&settings.connections, selection);
    if inputs.is_empty() {
        return;
    }
    hub.update_connections("音声認識モデルの割当", settings.revision, inputs, None, cx);
}

pub(crate) fn assign_tts_voice(
    hub: &mut Hub,
    selection: Option<(&str, &str)>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    if !hub.rpc.has_host() {
        return;
    }
    let inputs = build_tts_assignment_inputs(&settings.connections, selection);
    if inputs.is_empty() {
        return;
    }
    let target_voice_clean = selection
        .as_ref()
        .map(|(_, vid)| vid.trim().to_owned())
        .filter(|v| !v.is_empty());
    let probe_voicevox_id = selection.as_ref().and_then(|(cid, _)| {
        let conn = settings.connections.iter().find(|c| c.id == *cid)?;
        (conn.is_local_voicevox()
            && (!conn.enabled || hub.model.voice_choices_for_connection(&conn.id).len() <= 1))
            .then(|| conn.id.clone())
    });
    hub.update_connections(
        "音声合成（TTS）の割当",
        settings.revision,
        inputs,
        probe_voicevox_id,
        cx,
    );
    sync_companion_voice(hub, target_voice_clean);
}

fn sync_companion_voice(hub: &Hub, voice: Option<String>) {
    let Some(voice_clean) = voice else {
        return;
    };
    let Some(comp) = hub.model.companion() else {
        return;
    };
    if comp.voice_identifier.as_deref() == Some(voice_clean.as_str()) {
        return;
    }
    rpc::companions::sync_companion_voice(
        &hub.rpc,
        hub.model.connections.link_epoch,
        comp.id.clone(),
        comp.revision,
        voice_clean,
    );
}
