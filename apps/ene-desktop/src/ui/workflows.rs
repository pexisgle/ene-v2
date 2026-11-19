//! Standalone RPC workflows extracted from `Hub` so connection setup,
//! verification, and template import/export/preview can be tested without GPUI.

use super::{decode, failure};
use crate::{host::Host, update::Update};
use buffa::view::MessageView;
use ene_rpc as w;

pub(crate) const TEMPLATE_FORMAT_VERSION: u32 = 1;

pub(crate) fn export_template_json(template: &w::CompanionTemplate) -> Result<String, String> {
    let mut sanitized = template.clone();
    if sanitized.format_version == 0 {
        sanitized.format_version = TEMPLATE_FORMAT_VERSION;
    }
    sanitize_template(&mut sanitized);
    serde_json::to_string_pretty(&sanitized)
        .map_err(|error| format!("テンプレートの書き出しに失敗しました: {error}"))
}

pub(crate) fn parse_template_json(raw: &str) -> Result<w::CompanionTemplate, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("テンプレート JSON を入力してください".into());
    }
    let mut template: w::CompanionTemplate = serde_json::from_str(trimmed)
        .map_err(|error| format!("テンプレート JSON の形式が正しくありません: {error}"))?;
    if template.format_version > TEMPLATE_FORMAT_VERSION {
        return Err(format!(
            "未対応のテンプレート形式バージョン ({}) です",
            template.format_version
        ));
    }
    if template.format_version == 0 {
        template.format_version = TEMPLATE_FORMAT_VERSION;
    }
    if template.name.trim().is_empty() {
        return Err("テンプレートにコンパニオン名が含まれていません".into());
    }
    sanitize_template(&mut template);
    Ok(template)
}

fn sanitize_template(template: &mut w::CompanionTemplate) {
    if let Some(mut voice) = template.voice.take() {
        if let Ok(mut binding) = serde_json::from_str::<w::VoiceBinding>(&voice.json) {
            binding.connection_id = None;
            if let Ok(cleaned) = serde_json::to_string(&binding) {
                voice.json = cleaned;
            }
        }
        template.voice = Some(voice).into();
    }
}

pub(crate) async fn quick_connect_llm(
    host: Host,
    revision: u64,
    id: String,
    create: bool,
    input: w::ConnectionInput,
    secret: String,
) -> Update {
    let outcome: Result<Update, connectrpc::ConnectError> = async {
        if create {
            host.settings
                .update_settings(w::UpdateSettingsRequest {
                    expected_revision: revision,
                    connections: vec![input],
                    ..Default::default()
                })
                .await?;
        }
        if !secret.is_empty() {
            host.settings
                .register_credentials(w::CredentialRequest {
                    id: id.clone(),
                    secret: Some(secret),
                    ..Default::default()
                })
                .await?;
        }
        let probe = host
            .settings
            .check_connection(w::CheckRequest {
                id: id.clone(),
                request_key: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            })
            .await?
            .view()
            .to_owned_message()
            .map_err(decode)?;
        let check = probe.check.into_option();
        let healthy = check.as_ref().is_some_and(|c| c.healthy);
        let models = check
            .as_ref()
            .map(|c| c.resources.clone())
            .unwrap_or_default();
        let error = check
            .as_ref()
            .and_then(|c| c.failure.as_option())
            .map(|f| f.message.clone())
            .filter(|msg| !msg.trim().is_empty());
        Ok(Update::SetupFinished {
            ok: healthy,
            connection_id: Some(id),
            models,
            error,
        })
    }
    .await;
    match outcome {
        Ok(update) => update,
        Err(err) => Update::SetupFinished {
            ok: false,
            connection_id: None,
            models: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

pub(crate) struct SetupSelection {
    pub(crate) settings_revision: u64,
    pub(crate) conn: Option<w::Connection>,
    pub(crate) connection_id: String,
    pub(crate) model_id: String,
    pub(crate) existing_comp: Option<w::Companion>,
    pub(crate) companion_name: String,
    pub(crate) voice_identifier: Option<String>,
}

pub(crate) async fn complete_setup_selection(
    host: Host,
    input: SetupSelection,
) -> Result<Update, connectrpc::ConnectError> {
    let SetupSelection {
        settings_revision,
        conn,
        connection_id,
        model_id,
        existing_comp,
        companion_name,
        voice_identifier,
    } = input;
    if let Some(conn) = conn {
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        if let Some(w::connection_config::Config::Llm(llm)) = cfg.config.as_mut() {
            llm.model_ids = vec![model_id.clone()];
        }
        let input = w::ConnectionInput {
            id: conn.id.clone(),
            name: conn.name,
            enabled: true,
            expected_revision: conn.revision,
            kind: conn.kind,
            config: Some(cfg).into(),
            ..Default::default()
        };
        let _ = host
            .settings
            .update_settings(w::UpdateSettingsRequest {
                expected_revision: settings_revision,
                connections: vec![input],
                ..Default::default()
            })
            .await?;
    }
    let patch = w::AssignmentPatch {
        change: Some(w::assignment_patch::Change::Set(Box::new(
            w::ModelAssignment {
                connection_id: connection_id.clone(),
                model_id: model_id.clone(),
                ..Default::default()
            },
        ))),
        ..Default::default()
    };
    if let Some(comp) = existing_comp {
        let response = host
            .host
            .update_companion(w::UpdateCompanionRequest {
                id: comp.id,
                revision: comp.revision,
                companion_assignment: Some(patch.clone()).into(),
                task_assignment: Some(patch).into(),
                voice_identifier,
                ..Default::default()
            })
            .await?;
        let updated = response.view().to_owned_message().map_err(decode)?;
        Ok(Update::Notification(Box::new(w::Notification {
            kind: "companion".into(),
            event: Some(w::notification::Event::Companion(Box::new(updated))),
            ..Default::default()
        })))
    } else {
        let assignments = w::ModelAssignments {
            companion: Some(w::ModelAssignment {
                connection_id: connection_id.clone(),
                model_id: model_id.clone(),
                ..Default::default()
            })
            .into(),
            task: Some(w::ModelAssignment {
                connection_id,
                model_id,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let response = host
            .host
            .create_companion(w::CreateCompanionRequest {
                name: companion_name,
                mind: "相手を思いやりながら率直に対話する。".into(),
                persona: Some("柔らかく親しみやすい口調で会話します。".into()),
                voice_identifier,
                assignments: Some(assignments).into(),
                ..Default::default()
            })
            .await?;
        let created = response.view().to_owned_message().map_err(decode)?;
        Ok(Update::Created(created.id))
    }
}

pub(crate) async fn probe_connection(host: Host, connection_id: String) -> Update {
    match host
        .settings
        .check_connection(w::CheckRequest {
            id: connection_id.clone(),
            request_key: uuid::Uuid::new_v4().to_string(),
            ..Default::default()
        })
        .await
    {
        Ok(resp) => match resp.view().to_owned_message() {
            Ok(res) => Update::Checked {
                connection_id,
                check: res.check.into_option().unwrap_or_default(),
            },
            Err(err) => failure("接続確認", decode(err)),
        },
        Err(err) => failure("接続確認", err),
    }
}

pub(crate) async fn list_companion_templates(
    host: Host,
) -> Result<Vec<w::CompanionTemplate>, connectrpc::ConnectError> {
    let response = host
        .host
        .list_companion_templates(w::Empty::default())
        .await?;
    Ok(response.view().to_owned_message().map_err(decode)?.items)
}

pub(crate) async fn export_companion(
    host: Host,
    companion_id: String,
) -> Result<String, connectrpc::ConnectError> {
    let response = host
        .host
        .export_companion(w::Id {
            id: companion_id,
            ..Default::default()
        })
        .await?;
    let template = response.view().to_owned_message().map_err(decode)?;
    export_template_json(&template).map_err(connectrpc::ConnectError::internal)
}

pub(crate) async fn import_companion(
    host: Host,
    template: w::CompanionTemplate,
    assignments: Option<w::ModelAssignments>,
) -> Result<w::Companion, connectrpc::ConnectError> {
    let response = host
        .host
        .import_companion(w::ImportCompanionRequest {
            template: Some(template).into(),
            assignments: assignments.into(),
            ..Default::default()
        })
        .await?;
    response.view().to_owned_message().map_err(decode)
}

pub(crate) async fn preview_companion(host: Host, request: w::PreviewCompanionRequest) -> Update {
    match host.host.preview_companion(request).await {
        Ok(resp) => match resp.view().to_owned_message() {
            Ok(body) => Update::CompanionPreview {
                reply: Some(body.reply),
                error: None,
            },
            Err(err) => Update::CompanionPreview {
                reply: None,
                error: Some(err.to_string()),
            },
        },
        Err(err) => Update::CompanionPreview {
            reply: None,
            error: Some(err.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f03_template_json_round_trip_strips_connection_id_and_rejects_bad_versions() {
        let template = w::CompanionTemplate {
            format_version: 1,
            id: "ene".into(),
            name: "エネ".into(),
            mind: "率直で優しい相棒。".into(),
            persona: Some("親しみやすい口調".into()),
            body: Some(w::JsonDocument {
                schema: "ene.body.v1".into(),
                json: serde_json::json!({"model": "ENE.vrm"}).to_string(),
                ..Default::default()
            })
            .into(),
            voice: Some(w::JsonDocument {
                schema: "ene.voice.v1".into(),
                json: serde_json::json!({"connection_id": "secret-conn", "voice_id": "3", "speed": 1.1})
                    .to_string(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let json = export_template_json(&template).unwrap();
        let parsed = parse_template_json(&json).unwrap();
        let voice: w::VoiceBinding =
            serde_json::from_str(&parsed.voice.as_option().unwrap().json).unwrap();
        assert_eq!(voice.connection_id, None);
        assert_eq!(voice.voice_id.as_deref(), Some("3"));

        let bad_ver =
            serde_json::json!({"format_version": 99, "name": "x", "mind": "y"}).to_string();
        assert!(parse_template_json(&bad_ver).is_err());
        assert!(parse_template_json("").is_err());
    }
}
