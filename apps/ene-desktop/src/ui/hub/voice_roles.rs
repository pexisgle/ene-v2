//! Default STT and TTS connection assignments for [`Hub`].

use super::Hub;
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
    let probe_voicevox_id = selection.as_ref().and_then(|(cid, _)| {
        let conn = settings.connection(cid)?;
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
}
