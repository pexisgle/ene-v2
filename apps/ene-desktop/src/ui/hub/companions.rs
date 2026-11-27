//! Companion profile, template, preview, model role, and mind helpers for [`Hub`].

use super::Hub;
use crate::{rpc, state::NoticeKind};
use ene_client_core::{
    ModelRoleSlot,
    domain::{
        CompanionProfilePatch, CompanionTemplate, MindProfile, ModelAssignment, NewCompanionSpec,
    },
    templates::parse_template_json,
};
use gpui::Context;

pub(super) fn create_companion(hub: &mut Hub, spec: NewCompanionSpec, cx: &mut Context<Hub>) {
    rpc::companions::create_companion(&hub.rpc, hub.model.connections.link_epoch, spec);
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
    rpc::companions::import_companion(&hub.rpc, hub.model.connections.link_epoch, template);
    cx.notify();
}

pub(super) fn preview_companion(
    hub: &mut Hub,
    spec: NewCompanionSpec,
    model: ModelAssignment,
    revision: u64,
    cx: &mut Context<Hub>,
) -> bool {
    let admitted = rpc::companions::preview_companion(
        &hub.rpc,
        hub.model.connections.link_epoch,
        spec,
        model,
        revision,
    );
    if admitted {
        hub.model.companions.preview_reply = None;
        cx.notify();
    }
    admitted
}

pub(super) fn assign_model(
    hub: &mut Hub,
    slot: ModelRoleSlot,
    assignment: Option<ModelAssignment>,
    cx: &mut Context<Hub>,
) {
    let Some(companion) = hub.model.companion() else {
        return;
    };
    rpc::companions::assign_model(
        &hub.rpc,
        hub.model.connections.link_epoch,
        companion.id.clone(),
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
