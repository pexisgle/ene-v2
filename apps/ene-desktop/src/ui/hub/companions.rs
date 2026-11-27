//! Companion profile, template, preview, model role, and mind helpers for [`Hub`].

use super::Hub;
use crate::{rpc, state::NoticeKind};
use ene_client_core::{
    ModelRoleSlot,
    domain::{
        CompanionProfilePatch, CompanionTemplate, ConnectionConfig, ConnectionKind, MindProfile,
        ModelAssignment, NewCompanionSpec,
    },
    templates::parse_template_json,
};
use gpui::Context;

fn default_model(hub: &Hub) -> Option<ModelAssignment> {
    hub.model
        .model_choices()
        .into_iter()
        .next()
        .map(|(connection_id, model_id, _)| ModelAssignment {
            connection_id,
            model_id,
        })
}

pub(super) fn create_companion(hub: &mut Hub, spec: NewCompanionSpec, cx: &mut Context<Hub>) {
    let default_model = default_model(hub);
    let voice = spec
        .voice_identifier
        .clone()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            hub.model.settings().and_then(|settings| {
                settings
                    .connections
                    .iter()
                    .find(|connection| connection.kind == ConnectionKind::Tts)
                    .and_then(|connection| connection.config.as_ref())
                    .and_then(|config| match config {
                        ConnectionConfig::Tts(tts) => tts.voice_ids.first().cloned(),
                        _ => None,
                    })
            })
        });
    rpc::companions::create_companion(
        &hub.rpc,
        hub.model.connections.link_epoch,
        spec,
        voice,
        default_model,
    );
    cx.notify();
}

pub(super) fn update_companion_profile(
    hub: &mut Hub,
    patch: CompanionProfilePatch,
    cx: &mut Context<Hub>,
) {
    let Some(companion) = hub
        .model
        .snapshot()
        .and_then(|s| s.companions.iter().find(|c| c.id == patch.companion_id))
        .cloned()
    else {
        return;
    };
    rpc::companions::update_companion_profile(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion,
        patch,
    );
    cx.notify();
}

pub(super) fn import_companion_json(hub: &mut Hub, raw: &str, cx: &mut Context<Hub>) {
    let template = match parse_template_json(raw) {
        Ok(template) => template,
        Err(error) => {
            hub.model.notice(
                NoticeKind::Failed,
                "テンプレートの読み込み",
                error.to_string(),
            );
            cx.notify();
            return;
        }
    };
    import_companion_template(hub, template, cx);
}

pub(super) fn import_companion_template(
    hub: &mut Hub,
    template: CompanionTemplate,
    cx: &mut Context<Hub>,
) {
    let default_model = default_model(hub);
    rpc::companions::import_companion(
        &hub.rpc,
        hub.model.connections.link_epoch,
        template,
        default_model,
    );
    cx.notify();
}

pub(super) fn preview_companion(
    hub: &mut Hub,
    name: String,
    mind: String,
    persona: String,
    cx: &mut Context<Hub>,
) {
    if hub.rpc.host().is_none() {
        return;
    }
    hub.model.companions.preview_reply = None;
    let model = default_model(hub);
    rpc::companions::preview_companion(
        &hub.rpc,
        hub.model.connections.link_epoch,
        name,
        mind,
        persona,
        model,
    );
    cx.notify();
}

pub(super) fn assign_model(
    hub: &mut Hub,
    slot: ModelRoleSlot,
    assignment: Option<(String, String)>,
    cx: &mut Context<Hub>,
) {
    let Some(companion) = hub.model.companion().cloned() else {
        return;
    };
    let assignment = assignment.map(|(connection_id, model_id)| ModelAssignment {
        connection_id,
        model_id,
    });
    rpc::companions::assign_model(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion.id,
        companion.revision,
        slot,
        assignment,
    );
    cx.notify();
}

pub(super) fn update_mind_profile(
    hub: &mut Hub,
    companion_id: String,
    expected_revision: u64,
    profile: MindProfile,
    cx: &mut Context<Hub>,
) {
    rpc::companions::update_mind_profile(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion_id,
        expected_revision,
        profile,
    );
    cx.notify();
}
