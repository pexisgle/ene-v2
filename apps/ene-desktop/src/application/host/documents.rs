use ene_rpc::{Companion, JsonDocument, JsonDocumentPatch, Snapshot, json_document_patch::Change};
use serde_json::{Value, json};

/// Document values and choices displayed when editing started. Notifications
/// must not replace this baseline while a companion draft is dirty.
#[derive(Default)]
pub(in crate::application) struct CompanionDocuments {
    environment: String,
    body: Option<JsonDocument>,
    voice: Option<JsonDocument>,
    pub voices: Vec<(String, String)>,
}

impl CompanionDocuments {
    pub fn new(snapshot: &Snapshot, companion: &Companion) -> Self {
        let mut draft = Self {
            environment: snapshot.environment_id.clone(),
            body: companion.body.as_option().cloned(),
            voice: companion.voice.as_option().cloned(),
            voices: vec![(String::new(), "未設定".into())],
        };
        draft.voices.extend(
            snapshot
                .settings
                .connections
                .iter()
                .filter(|c| c.kind == "tts")
                .map(|c| (c.id.clone(), format!("{} · {}", c.adapter, c.endpoint))),
        );
        let saved = field(draft.voice.as_ref(), "connection_id");
        if !saved.is_empty() && !draft.voices.iter().any(|(id, _)| *id == saved) {
            draft
                .voices
                .push((saved, "保存済みの接続（現在利用不可）".into()));
        }
        draft
    }
    pub fn model(&self) -> String {
        field(self.body.as_ref(), "model")
    }
    pub fn voice_id(&self) -> String {
        field(self.voice.as_ref(), "voice_id")
    }
    pub fn voice_index(&self) -> i32 {
        let saved = field(self.voice.as_ref(), "connection_id");
        self.voices
            .iter()
            .position(|(id, _)| *id == saved)
            .unwrap_or(0) as i32
    }
    pub fn patches(
        &self,
        model: &str,
        voice_index: i32,
        voice_id: &str,
    ) -> Result<(Option<JsonDocumentPatch>, Option<JsonDocumentPatch>), &'static str> {
        let model = model.trim();
        let body = if model == self.model() {
            None
        } else if model.is_empty() {
            Some(clear())
        } else {
            let path = std::path::Path::new(model);
            if !path.is_file()
                || !path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("vrm"))
            {
                return Err("この端末にある .vrm ファイルのパスを指定してください。");
            }
            Some(set(
                self.body.as_ref(),
                json!({"format":"vrm1", "environment_id":self.environment, "model":model}),
            ))
        };
        let (connection, _) = usize::try_from(voice_index)
            .ok()
            .and_then(|i| self.voices.get(i))
            .ok_or("音声合成サービスを選び直してください。")?;
        let voice = if connection.is_empty() {
            if self.voice_index() == 0 {
                None
            } else {
                Some(clear())
            }
        } else if *connection == field(self.voice.as_ref(), "connection_id")
            && voice_id.trim() == self.voice_id()
        {
            None
        } else {
            if voice_id.trim().is_empty() || voice_id.chars().any(char::is_control) {
                return Err("音声合成サービスの声IDを指定してください。");
            }
            Some(set(
                self.voice.as_ref(),
                json!({"connection_id":connection, "voice_id":voice_id.trim()}),
            ))
        };
        Ok((body, voice))
    }
}

fn field(document: Option<&JsonDocument>, name: &str) -> String {
    document
        .and_then(|d| serde_json::from_str::<Value>(&d.json).ok())
        .and_then(|value| value.get(name)?.as_str().map(str::to_owned))
        .unwrap_or_default()
}
fn clear() -> JsonDocumentPatch {
    JsonDocumentPatch {
        change: Some(Change::Clear(Box::default())),
        ..Default::default()
    }
}
fn set(original: Option<&JsonDocument>, fields: Value) -> JsonDocumentPatch {
    let mut object = original
        .and_then(|d| serde_json::from_str::<Value>(&d.json).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    object.extend(fields.as_object().expect("document fields").clone());
    JsonDocumentPatch {
        change: Some(Change::Set(Box::new(JsonDocument {
            json: Value::Object(object).to_string(),
            ..Default::default()
        }))),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_edits_preserve_opaque_documents_from_existing_storage() {
        let companion = Companion {
            body: JsonDocument {
                json: r#"{"legacy_model":"saved"}"#.into(),
                ..Default::default()
            }
            .into(),
            voice: JsonDocument {
                json: r#"{"legacy_voice":"saved"}"#.into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        let draft = CompanionDocuments::new(&Snapshot::default(), &companion);
        let (body, voice) = draft
            .patches(&draft.model(), draft.voice_index(), &draft.voice_id())
            .unwrap();
        assert!(body.is_none() && voice.is_none());
    }
}
