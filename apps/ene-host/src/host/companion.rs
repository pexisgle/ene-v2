//! A companion's authored identity and evolving mind.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Profile {
    #[serde(default)]
    pub(crate) facets: Vec<String>,
    #[serde(default)]
    pub(crate) examples: Vec<String>,
    #[serde(default)]
    pub(crate) background: Vec<String>,
    #[serde(default)]
    pub(crate) expression: Vec<String>,
}

impl Profile {
    pub(crate) fn sections(&self) -> [(&'static str, &[String]); 4] {
        [
            ("facets", &self.facets),
            ("examples", &self.examples),
            ("background", &self.background),
            ("expression", &self.expression),
        ]
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct MindState {
    #[serde(default)]
    pub(crate) emotion: Value,
    #[serde(default)]
    pub(crate) relationships: Vec<Value>,
    #[serde(default)]
    pub(crate) tendencies: Vec<String>,
    #[serde(default)]
    pub(crate) concerns: Vec<String>,
}

impl MindState {
    pub(crate) fn is_empty(&self) -> bool {
        self.emotion.is_null()
            && self.relationships.is_empty()
            && self.tendencies.is_empty()
            && self.concerns.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ModelAssignment {
    pub(crate) connection: String,
    pub(crate) model: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Assignments {
    pub(crate) companion: Option<ModelAssignment>,
    pub(crate) task: Option<ModelAssignment>,
    pub(crate) perception: Option<ModelAssignment>,
    pub(crate) compaction: Option<ModelAssignment>,
    pub(crate) consolidation: Option<ModelAssignment>,
    pub(crate) decision: Option<ModelAssignment>,
    pub(crate) embedding: Option<ModelAssignment>,
}

#[derive(Clone, Debug)]
pub(crate) struct Companion {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) mind: String,
    pub(crate) body: Value,
    pub(crate) voice: Value,
    pub(crate) assignments: Assignments,
    pub(crate) profile: Profile,
    pub(crate) state: MindState,
    pub(crate) persona: Option<String>,
    pub(crate) voice_identifier: Option<String>,
    pub(crate) revision: u64,
    pub(crate) profile_revision: u64,
    pub(crate) state_revision: u64,
    pub(crate) memory_revision: u64,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

impl Companion {
    pub(crate) fn seed(id: String, now: String) -> Self {
        Self {
            id,
            name: "ene".into(),
            mind: "日本語で、簡潔で親しみやすく応答する。".into(),
            body: json!({}),
            voice: json!({}),
            assignments: Assignments::default(),
            profile: Profile::default(),
            state: MindState::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            profile_revision: 1,
            state_revision: 1,
            memory_revision: 1,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}
