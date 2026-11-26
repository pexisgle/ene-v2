use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Default, Deserialize, Serialize)]
pub(super) struct SpeechOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<Speaker>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<String>,
    #[serde(flatten)]
    other: BTreeMap<String, serde_json::Value>,
}

impl SpeechOptions {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).unwrap_or_default()
    }

    pub fn set_sakura_voice(&mut self, selection: &str) {
        let (model, voice) = selection.split_once('/').unwrap_or((selection, ""));
        if let Some(model) = nonempty(model) {
            self.model = Some(model.to_owned());
        }
        if let Some(voice) = nonempty(voice) {
            self.voice = Some(voice.to_owned());
        }
        self.protocol.get_or_insert_with(|| "sakura".to_owned());
        self.response_format.get_or_insert_with(|| "wav".to_owned());
    }

    pub fn voice_id(&self, sakura: bool) -> Option<String> {
        let voice = self.voice.as_deref().and_then(nonempty);
        if sakura {
            let model = self
                .model
                .as_deref()
                .and_then(nonempty)
                .unwrap_or("zundamon");
            return Some(format!("{model}/{}", voice.unwrap_or("normal")));
        }
        match &self.speaker {
            Some(Speaker::Id(id)) => Some(id.to_string()),
            Some(Speaker::Name(name)) => nonempty(name).or(voice).map(str::to_owned),
            None => voice.map(str::to_owned),
        }
    }

    pub fn json(&self) -> String {
        serde_json::json!(self).to_string()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum Speaker {
    Id(u64),
    Name(String),
}

pub(super) fn nonempty(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}
