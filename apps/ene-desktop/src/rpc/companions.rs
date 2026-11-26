//! Companion CRUD, model assignment, mind profile, template, and preview RPCs.

use super::{RpcDispatcher, companion_update, id_request};
use crate::update::Update;
use ene_client_core::{
    ModelRoleSlot,
    domain::{
        AutoAssignModelPatch, Companion, CompanionProfilePatch, CompanionTemplate, MindProfile,
        ModelAssignment, NewCompanionSpec,
    },
    templates::export_template_json,
};
use ene_rpc as w;

pub(super) fn shared_model_assignments(value: ModelAssignment) -> w::ModelAssignments {
    let assignment = w::ModelAssignment::from(value);
    w::ModelAssignments {
        companion: Some(assignment.clone()).into(),
        task: Some(assignment).into(),
        ..Default::default()
    }
}

fn update_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    context: &'static str,
    request: w::UpdateCompanionRequest,
) {
    rpc.call_owned(
        epoch,
        context,
        request!(host.update_companion(request)),
        companion_update,
    );
}

fn binding_document(schema: &str, value: &impl serde::Serialize) -> Option<w::JsonDocument> {
    serde_json::to_string(value)
        .ok()
        .map(|json| w::JsonDocument {
            schema: schema.into(),
            json,
            ..Default::default()
        })
}

pub(crate) fn create_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    spec: NewCompanionSpec,
    voice: Option<String>,
    default_model: Option<ModelAssignment>,
) {
    let assignments = default_model.map(shared_model_assignments);
    rpc.call_owned(
        epoch,
        "コンパニオンの作成",
        request!(host.create_companion(w::CreateCompanionRequest {
            name: spec.name,
            mind: spec.mind,
            persona: (!spec.persona.trim().is_empty()).then_some(spec.persona),
            voice_identifier: voice,
            assignments: assignments.into(),
            ..Default::default()
        })),
        |companion| Update::Created(companion.id),
    );
}

pub(crate) fn update_companion_profile(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion: Companion,
    patch: CompanionProfilePatch,
) {
    let voice_identifier = patch.voice.voice_id.clone().unwrap_or_default();
    let body_doc = binding_document("ene.body.v1", &patch.body);
    let voice_doc = binding_document("ene.voice.v1", &patch.voice);
    update_companion(
        rpc,
        epoch,
        "コンパニオンの保存",
        w::UpdateCompanionRequest {
            id: companion.id,
            revision: companion.revision,
            name: (!patch.name.is_empty() && patch.name != companion.name).then_some(patch.name),
            persona: Some(patch.persona),
            mind: (patch.mind != companion.mind).then_some(patch.mind),
            body: body_doc.into(),
            voice: voice_doc.into(),
            voice_identifier: Some(voice_identifier),
            ..Default::default()
        },
    );
}

pub(crate) fn assign_model(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    revision: u64,
    slot: ModelRoleSlot,
    assignment: Option<ModelAssignment>,
) {
    let patch = w::AssignmentPatch::from(assignment);
    let mut request = w::UpdateCompanionRequest {
        id: companion_id,
        revision,
        ..Default::default()
    };
    match slot {
        ModelRoleSlot::Companion => request.companion_assignment = Some(patch).into(),
        ModelRoleSlot::Task => request.task_assignment = Some(patch).into(),
        ModelRoleSlot::Perception => request.perception_assignment = Some(patch).into(),
        ModelRoleSlot::Compaction => request.compaction_assignment = Some(patch).into(),
        ModelRoleSlot::Consolidation => request.consolidation_assignment = Some(patch).into(),
        ModelRoleSlot::Decision => request.decision_assignment = Some(patch).into(),
        ModelRoleSlot::Embedding => request.embedding_assignment = Some(patch).into(),
    }
    update_companion(rpc, epoch, "モデル割当の変更", request);
}

pub(crate) fn auto_assign_default_model(
    rpc: &RpcDispatcher,
    epoch: u64,
    patch: AutoAssignModelPatch,
) {
    let wire_patch = w::AssignmentPatch::from(Some(patch.assignment));
    let request = w::UpdateCompanionRequest {
        id: patch.companion_id,
        revision: patch.revision,
        companion_assignment: patch.assign_companion.then(|| wire_patch.clone()).into(),
        task_assignment: patch.assign_task.then_some(wire_patch).into(),
        ..Default::default()
    };
    update_companion(rpc, epoch, "既定モデルの自動設定", request);
}

pub(crate) fn sync_companion_voice(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    revision: u64,
    voice_clean: String,
) {
    update_companion(
        rpc,
        epoch,
        "コンパニオン話者の同期",
        w::UpdateCompanionRequest {
            id: companion_id,
            revision,
            voice_identifier: Some(voice_clean),
            ..Default::default()
        },
    );
}

pub(crate) fn delete_companion(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let gone_id = companion_id.clone();
    rpc.call(
        epoch,
        "コンパニオンの削除",
        request!(host.delete_companion(id_request(companion_id))),
        move |_| Update::Gone {
            companion_id: gone_id,
        },
    );
}

load! {
    load_companion_templates: host.list_companion_templates("テンプレート一覧の取得") =>
        |templates| Update::CompanionTemplates(templates.items.into_iter().map(Into::into).collect());
}

pub(crate) fn export_companion(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let target = companion_id.clone();
    rpc.call(
        epoch,
        "テンプレートの書き出し",
        |host| async move {
            let template = host
                .host
                .export_companion(id_request(companion_id))
                .await?
                .into_owned();
            export_template_json(&template.into())
                .map_err(|error| connectrpc::ConnectError::internal(error.to_string()))
        },
        move |json| Update::CompanionExported {
            companion_id: target,
            json,
        },
    );
}

pub(crate) fn import_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    template: CompanionTemplate,
    default_model: Option<ModelAssignment>,
) {
    let assignments = default_model.map(shared_model_assignments);
    rpc.call_owned(
        epoch,
        "テンプレートから作成",
        request!(host.import_companion(w::ImportCompanionRequest {
            template: Some(template.into()).into(),
            assignments: assignments.into(),
            ..Default::default()
        })),
        |companion| Update::Created(companion.id),
    );
}

pub(crate) fn preview_companion(
    rpc: &RpcDispatcher,
    epoch: u64,
    name: String,
    mind: String,
    persona: String,
    model: Option<ModelAssignment>,
) {
    let request = w::PreviewCompanionRequest {
        name,
        mind,
        persona: (!persona.trim().is_empty()).then_some(persona),
        model: model.map(Into::into).into(),
        user_message: "はじめまして！短く自己紹介してください。".into(),
        ..Default::default()
    };
    rpc.call_update(epoch, |host| async move {
        match host.host.preview_companion(request).await {
            Ok(resp) => Update::CompanionPreview {
                reply: Some(resp.view().reply.to_owned()),
                error: None,
            },
            Err(err) => Update::CompanionPreview {
                reply: None,
                error: Some(err.to_string()),
            },
        }
    });
}

pub(crate) fn load_mind(rpc: &RpcDispatcher, epoch: u64, companion_id: String) {
    let target = companion_id.clone();
    rpc.call_owned(
        epoch,
        "詳細プロフィールの取得",
        request!(host.get_mind(id_request(companion_id))),
        move |view| Update::MindLoaded {
            companion_id: target,
            view: Box::new(view.into()),
        },
    );
}

pub(crate) fn update_mind_profile(
    rpc: &RpcDispatcher,
    epoch: u64,
    companion_id: String,
    expected_revision: u64,
    profile: MindProfile,
) {
    let target = companion_id.clone();
    rpc.call_owned(
        epoch,
        "詳細プロフィールの保存",
        request!(host.update_mind_profile(w::UpdateMindProfileRequest {
            companion_id,
            expected_revision,
            profile: Some(profile.into()).into(),
            ..Default::default()
        })),
        move |view| Update::MindLoaded {
            companion_id: target,
            view: Box::new(view.into()),
        },
    );
}
