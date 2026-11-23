//! Pure companion template JSON serialization, validation, and sanitization.

use crate::domain::{CompanionTemplate, VoiceBinding};

/// Supported companion template JSON schema format version.
pub const TEMPLATE_FORMAT_VERSION: u32 = 1;

/// Failures when exporting or importing companion JSON templates.
#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("テンプレートの書き出しに失敗しました: {0}")]
    Export(serde_json::Error),
    #[error("テンプレート JSON を入力してください")]
    EmptyInput,
    #[error("テンプレート JSON の形式が正しくありません: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("未対応のテンプレート形式バージョン ({0}) です")]
    UnsupportedVersion(u32),
    #[error("テンプレートにコンパニオン名が含まれていません")]
    MissingName,
}

/// Serializes and sanitizes a companion template into wire-compatible pretty JSON.
pub fn export_template_json(template: &CompanionTemplate) -> Result<String, TemplateError> {
    let mut sanitized = template.clone();
    if sanitized.format_version == 0 {
        sanitized.format_version = TEMPLATE_FORMAT_VERSION;
    }
    sanitize_template(&mut sanitized);
    serde_json::to_string_pretty(&sanitized).map_err(TemplateError::Export)
}

/// Parses, validates, and sanitizes a companion template from JSON.
pub fn parse_template_json(raw: &str) -> Result<CompanionTemplate, TemplateError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TemplateError::EmptyInput);
    }
    let mut template: CompanionTemplate = serde_json::from_str(trimmed)?;
    if template.format_version > TEMPLATE_FORMAT_VERSION {
        return Err(TemplateError::UnsupportedVersion(template.format_version));
    }
    if template.format_version == 0 {
        template.format_version = TEMPLATE_FORMAT_VERSION;
    }
    if template.name.trim().is_empty() {
        return Err(TemplateError::MissingName);
    }
    sanitize_template(&mut template);
    Ok(template)
}

fn sanitize_template(template: &mut CompanionTemplate) {
    if let Some(voice) = template.voice.as_mut()
        && let Ok(mut binding) = serde_json::from_str::<VoiceBinding>(&voice.json)
    {
        binding.connection_id = None;
        if let Ok(cleaned) = serde_json::to_string(&binding) {
            voice.json = cleaned;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::JsonDocument;

    #[test]
    fn f03_template_json_round_trip_strips_connection_id_and_rejects_bad_versions() {
        let template = CompanionTemplate {
            format_version: 1,
            id: "ene".into(),
            name: "エネ".into(),
            mind: "率直で優しい相棒。".into(),
            persona: Some("親しみやすい口調".into()),
            body: Some(JsonDocument {
                schema: "ene.body.v1".into(),
                json: serde_json::json!({"model": "ENE.vrm"}).to_string(),
            }),
            voice: Some(JsonDocument {
                schema: "ene.voice.v1".into(),
                json: serde_json::json!({"connection_id": "secret-conn", "voice_id": "3", "speed": 1.1})
                    .to_string(),
            }),
            ..Default::default()
        };
        let json = export_template_json(&template).unwrap();
        let parsed = parse_template_json(&json).unwrap();
        let voice: VoiceBinding =
            serde_json::from_str(&parsed.voice.as_ref().unwrap().json).unwrap();
        assert_eq!(voice.connection_id, None);
        assert_eq!(voice.voice_id.as_deref(), Some("3"));

        let bad_ver =
            serde_json::json!({"format_version": 99, "name": "x", "mind": "y"}).to_string();
        assert!(parse_template_json(&bad_ver).is_err());
        assert!(parse_template_json("").is_err());
    }
}
