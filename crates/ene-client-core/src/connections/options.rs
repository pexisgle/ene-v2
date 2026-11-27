use serde_json::{Map, Value};

#[derive(Default)]
pub(super) struct SpeechOptions(Map<String, Value>);

impl SpeechOptions {
    pub fn parse(raw: &str) -> Self {
        serde_json::from_str(raw).map_or_else(|_| Self::default(), Self)
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.0.insert(key.to_owned(), value.into());
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str).and_then(nonempty)
    }

    pub fn set_sakura_voice(&mut self, selection: &str) {
        let (model, voice) = selection.split_once('/').unwrap_or((selection, ""));
        for (key, value) in [("model", model), ("voice", voice)] {
            if let Some(value) = nonempty(value) {
                self.set(key, value);
            }
        }
        for (key, value) in [("protocol", "sakura"), ("response_format", "wav")] {
            if self.get(key).is_none_or(Value::is_null) {
                self.set(key, value);
            }
        }
    }

    pub fn voice_id(&self, sakura: bool) -> Option<String> {
        let voice = self.text("voice");
        if sakura {
            let model = self.text("model").unwrap_or("zundamon");
            return Some(format!("{model}/{}", voice.unwrap_or("normal")));
        }
        self.get("speaker")
            .and_then(Value::as_u64)
            .map(|id| id.to_string())
            .or_else(|| self.text("speaker").or(voice).map(str::to_owned))
    }

    pub fn json(self) -> String {
        Value::Object(self.0).to_string()
    }
}

pub(super) fn nonempty(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}
