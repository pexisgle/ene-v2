//! Companion profile, template, preview, model role, and mind operations on [`Hub`].

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

impl Hub {
    pub(crate) fn create_companion(&mut self, spec: NewCompanionSpec, cx: &mut Context<Self>) {
        let default_model = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(conn_id, model_id, _)| (conn_id, model_id));
        let voice = spec
            .voice_identifier
            .clone()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                self.model.settings().and_then(|settings| {
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
            &self.rpc,
            self.model.connections.link_epoch,
            spec,
            voice,
            default_model,
        );
        cx.notify();
    }

    pub(crate) fn update_companion_profile(
        &mut self,
        patch: CompanionProfilePatch,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self
            .model
            .snapshot()
            .and_then(|s| s.companions.iter().find(|c| c.id == patch.companion_id))
            .cloned()
        else {
            return;
        };
        rpc::companions::update_companion_profile(
            &self.rpc,
            self.model.connections.link_epoch,
            companion,
            patch,
        );
        cx.notify();
    }

    pub(crate) fn load_companion_templates(&mut self, _cx: &mut Context<Self>) {
        rpc::companions::load_companion_templates(&self.rpc, self.model.connections.link_epoch);
    }

    pub(crate) fn export_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        rpc::companions::export_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id,
        );
        cx.notify();
    }

    pub(crate) fn import_companion_json(&mut self, raw: String, cx: &mut Context<Self>) {
        let template = match parse_template_json(&raw) {
            Ok(template) => template,
            Err(error) => {
                self.model.notice(
                    NoticeKind::Failed,
                    "テンプレートの読み込み",
                    error.to_string(),
                );
                cx.notify();
                return;
            }
        };
        self.import_companion_template(template, cx);
    }

    pub(crate) fn import_companion_template(
        &mut self,
        template: CompanionTemplate,
        cx: &mut Context<Self>,
    ) {
        let default_model = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(conn_id, model_id, _)| (conn_id, model_id));
        rpc::companions::import_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            template,
            default_model,
        );
        cx.notify();
    }

    pub(crate) fn preview_companion(
        &mut self,
        name: String,
        mind: String,
        persona: String,
        cx: &mut Context<Self>,
    ) {
        if self.rpc.host().is_none() {
            return;
        }
        self.model.companions.preview_reply = None;
        let model = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(connection_id, model_id, _)| ModelAssignment {
                connection_id,
                model_id,
            });
        rpc::companions::preview_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            name,
            mind,
            persona,
            model,
        );
        cx.notify();
    }

    pub(crate) fn assign_model(
        &mut self,
        slot: ModelRoleSlot,
        assignment: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let assignment = assignment.map(|(connection_id, model_id)| ModelAssignment {
            connection_id,
            model_id,
        });
        rpc::companions::assign_model(
            &self.rpc,
            self.model.connections.link_epoch,
            companion.id,
            companion.revision,
            slot,
            assignment,
        );
        cx.notify();
    }

    pub(crate) fn delete_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        rpc::companions::delete_companion(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id,
        );
        cx.notify();
    }

    pub(crate) fn load_mind(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.load_mind_for(companion_id);
    }

    pub(crate) fn load_mind_for(&mut self, companion_id: String) {
        rpc::companions::load_mind(&self.rpc, self.model.connections.link_epoch, companion_id);
    }

    pub(crate) fn update_mind_profile(
        &mut self,
        companion_id: String,
        expected_revision: u64,
        profile: MindProfile,
        cx: &mut Context<Self>,
    ) {
        rpc::companions::update_mind_profile(
            &self.rpc,
            self.model.connections.link_epoch,
            companion_id,
            expected_revision,
            profile,
        );
        cx.notify();
    }
}
