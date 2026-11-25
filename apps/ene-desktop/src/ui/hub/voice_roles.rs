//! Model and voice selection, STT/TTS assignment, and companion voice sync for [`Hub`].

use super::Hub;
use crate::rpc;
use ene_client_core::{
    ModelRoleSlot,
    connections::{
        apply_stt_model_to_config, apply_tts_voice_to_config, build_connection_model_input,
        build_stt_assignment_inputs, build_tts_assignment_inputs,
    },
};
use gpui::Context;

pub(super) fn select_connection_model(
    hub: &mut Hub,
    connection_id: &str,
    model_id: &str,
    cx: &mut Context<Hub>,
) {
    let Some(conn) = hub.model.settings().and_then(|s| {
        s.connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
    }) else {
        return;
    };
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return;
    }
    hub.upsert_connection(
        "使用モデルの変更",
        build_connection_model_input(&conn, model_id),
        cx,
    );
    if let Some(comp) = hub.model.companion().cloned() {
        let assignments = comp.assignments.as_ref();
        let comp_uses_conn = assignments
            .and_then(|a| a.companion.as_ref())
            .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
        let task_uses_conn = assignments
            .and_then(|a| a.task.as_ref())
            .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
        if comp_uses_conn {
            hub.assign_model(
                ModelRoleSlot::Companion,
                Some((connection_id.to_owned(), model_id.to_owned())),
                cx,
            );
        }
        if task_uses_conn {
            hub.assign_model(
                ModelRoleSlot::Task,
                Some((connection_id.to_owned(), model_id.to_owned())),
                cx,
            );
        }
    }
}

pub(super) fn select_voicevox_speaker(
    hub: &mut Hub,
    connection_id: String,
    speaker_id: String,
    cx: &mut Context<Hub>,
) {
    select_tts_voice(hub, connection_id, speaker_id, cx);
}

pub(super) fn select_tts_voice(
    hub: &mut Hub,
    connection_id: String,
    voice_id: String,
    cx: &mut Context<Hub>,
) {
    let Some(conn) = hub.model.settings().and_then(|s| {
        s.connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
    }) else {
        return;
    };
    let voice_clean = voice_id.trim().to_owned();
    if voice_clean.is_empty() {
        return;
    }
    let is_sakura = conn.is_sakura();
    let mut input = conn.to_input();
    if let Some(cfg) = input.config.as_mut() {
        apply_tts_voice_to_config(cfg, is_sakura, &voice_clean);
    }
    hub.upsert_connection("音声話者の変更", input, cx);
}

pub(super) fn select_stt_model(
    hub: &mut Hub,
    connection_id: String,
    model_id: String,
    cx: &mut Context<Hub>,
) {
    let Some(conn) = hub.model.settings().and_then(|s| {
        s.connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
    }) else {
        return;
    };
    let model_clean = model_id.trim().to_owned();
    if model_clean.is_empty() {
        return;
    }
    let mut input = conn.to_input();
    if let Some(cfg) = input.config.as_mut() {
        apply_stt_model_to_config(cfg, &model_clean);
    }
    hub.upsert_connection("音声認識モデルの変更", input, cx);
}

pub(super) fn assign_stt_model(
    hub: &mut Hub,
    selection: Option<(String, String)>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    let inputs = build_stt_assignment_inputs(
        &settings.connections,
        selection
            .as_ref()
            .map(|(cid, mid)| (cid.as_str(), mid.as_str())),
    );
    if inputs.is_empty() {
        return;
    }
    rpc::connections::update_settings(
        &hub.rpc,
        hub.model.connections.link_epoch,
        "音声認識モデルの割当",
        settings.revision,
        inputs,
    );
    cx.notify();
}

pub(super) fn assign_tts_voice(
    hub: &mut Hub,
    selection: Option<(String, String)>,
    cx: &mut Context<Hub>,
) {
    let Some(settings) = hub.model.settings() else {
        return;
    };
    if hub.rpc.host().is_none() {
        return;
    }
    let inputs = build_tts_assignment_inputs(
        &settings.connections,
        selection
            .as_ref()
            .map(|(cid, vid)| (cid.as_str(), vid.as_str())),
    );
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
            && (!conn.enabled || hub.voice_choices_for_connection(&conn.id).len() <= 1))
            .then(|| conn.id.clone())
    });
    let epoch = hub.model.connections.link_epoch;
    if let Some(check_id) = probe_voicevox_id {
        hub.setup.checking_connections.insert(check_id.clone());
        rpc::connections::update_settings_and_probe(
            &hub.rpc,
            epoch,
            "音声合成（TTS）の割当",
            settings.revision,
            inputs,
            check_id,
        );
    } else {
        rpc::connections::update_settings(
            &hub.rpc,
            epoch,
            "音声合成（TTS）の割当",
            settings.revision,
            inputs,
        );
    }
    sync_companion_voice(hub, target_voice_clean);
    cx.notify();
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
