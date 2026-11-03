//! The companion's authored character material and lived mind.
//!
//! The author profile is role-play material, never a record of the user's
//! experience. A [`MindUpdateRequest`] may add memories, correct them with
//! references, and change the current mind; it cannot touch the profile or a
//! permission. The host binds the owner, operation id and input source.

use super::{Error, ErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a memory's content reached the companion. The distinction is stored,
/// not just described in prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Acquisition {
    /// The companion observed it in the accepted conversation.
    DirectExperience,
    /// The user stated it.
    UserStatement,
    /// Another agent or person reported it, not directly observed.
    Report,
    /// Shared through an allowed channel.
    Shared,
    /// The companion's own interpretation, not a confirmed fact.
    Inference,
}

/// One reason a memory exists. Stored as data; the host adds the current input
/// reference and validates any memory reference the model supplies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub kind: SourceKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// An accepted input; `id` is the host's request id.
    Input,
    /// Another memory used as evidence.
    Memory,
    /// An operation (a later consolidation job) that produced this.
    Operation,
}

/// A memory about to be added. The host fills the owner, id, revision, times
/// and current-input source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewMemory {
    pub content: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub appraisal: String,
    pub acquisition: Acquisition,
    /// Memory references whose content this memory corrects. They must belong
    /// to the same companion and are kept, not deleted.
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Extra memory references used as evidence. The host adds the input.
    #[serde(default)]
    pub sources: Vec<SourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
}

/// One stored memory, with its provenance and current correction links.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub owner: String,
    pub revision: u64,
    pub content: String,
    pub description: String,
    pub appraisal: String,
    pub acquisition: Acquisition,
    pub sources: Vec<SourceRef>,
    pub recorded_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    /// Memories that correct this one, computed from the owner's records.
    #[serde(default)]
    pub superseded_by: Vec<String>,
    pub recall_preference: String,
    pub created_by: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmotionSignal {
    pub label: String,
    pub intensity: f64,
}

/// The current feeling: what it is about, why, and how it may show. This is
/// separate from the emotion recorded inside a past memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Emotion {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub signals: Vec<EmotionSignal>,
}

/// How the companion understands a relationship, with memory evidence. Not a
/// single affinity number.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relationship {
    pub subject: String,
    pub understanding: String,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// A partial current-mind change. Omitted fields keep their value; a provided
/// relationship/tendency/concern list replaces its list.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emotion: Option<Emotion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relationships: Option<Vec<Relationship>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tendencies: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concerns: Option<Vec<String>>,
}

impl StateUpdate {
    pub fn is_empty(&self) -> bool {
        self.emotion.is_none()
            && self.relationships.is_none()
            && self.tendencies.is_none()
            && self.concerns.is_none()
    }
}

/// The mind change the parent requests. The operation id and owner are bound by
/// the host, not the model. Unknown fields (for example `profile` or
/// `permissions`) are rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MindUpdateRequest {
    pub expected_revision: u64,
    #[serde(default)]
    pub memories: Vec<NewMemory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateUpdate>,
}

impl MindUpdateRequest {
    pub fn is_empty(&self) -> bool {
        self.memories.is_empty() && self.state.as_ref().is_none_or(StateUpdate::is_empty)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangedMemory {
    pub id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MindUpdateResult {
    pub operation_id: String,
    pub revision: u64,
    pub changed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<ChangedMemory>,
}

/// The author's extra role-play material. `core` is stored on the companion.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDocument {
    #[serde(default)]
    pub facets: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
    #[serde(default)]
    pub background: Vec<String>,
    #[serde(default)]
    pub expression: Vec<String>,
}

/// The stored current mind. `emotion` stays a JSON object so the character's
/// own vocabulary is not flattened into a fixed scale.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredState {
    #[serde(default)]
    pub emotion: Option<Value>,
    #[serde(default)]
    pub relationships: Vec<Value>,
    #[serde(default)]
    pub tendencies: Vec<String>,
    #[serde(default)]
    pub concerns: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredMind {
    pub profile_revision: u64,
    pub profile: ProfileDocument,
    pub state_revision: u64,
    pub state: StoredState,
    pub updated_at: String,
}

const MAX_CONTENT: usize = 4000;
const MAX_TEXT: usize = 1000;
const MAX_ITEMS: usize = 16;
const MAX_PROFILE_ITEM: usize = 2000;

fn too_long(value: &str, max: usize) -> bool {
    value.chars().count() > max
}

fn check_string(value: &str, max: usize, reason: &'static str) -> Result<(), Error> {
    if value.trim().is_empty() || too_long(value, max) {
        return Err(Error {
            kind: ErrorKind::InvalidRequest,
            reason: Some(reason),
        });
    }
    Ok(())
}

fn check_optional(value: &Option<String>, max: usize, reason: &'static str) -> Result<(), Error> {
    if let Some(value) = value {
        check_string(value, max, reason)?;
    }
    Ok(())
}

fn check_items(
    items: &[String],
    max_items: usize,
    max_len: usize,
    reason: &'static str,
) -> Result<(), Error> {
    if items.len() > max_items {
        return Err(Error {
            kind: ErrorKind::InvalidRequest,
            reason: Some(reason),
        });
    }
    for item in items {
        check_string(item, max_len, reason)?;
    }
    Ok(())
}

impl MindUpdateRequest {
    pub fn validate(&self) -> Result<(), Error> {
        if self.memories.len() > MAX_ITEMS {
            return Err(Error::invalid());
        }
        for memory in &self.memories {
            check_string(&memory.content, MAX_CONTENT, "memory_too_long")?;
            if too_long(&memory.description, MAX_TEXT) || too_long(&memory.appraisal, MAX_TEXT) {
                return Err(Error {
                    kind: ErrorKind::InvalidRequest,
                    reason: Some("memory_text_too_long"),
                });
            }
            check_optional(&memory.occurred_at, 64, "occurred_at_too_long")?;
            if memory.supersedes.len() > MAX_ITEMS {
                return Err(Error::invalid());
            }
            for id in &memory.supersedes {
                check_string(id, 64, "invalid_reference")?;
            }
            if memory.sources.len() > MAX_ITEMS {
                return Err(Error::invalid());
            }
            for source in &memory.sources {
                if source.kind != SourceKind::Memory {
                    return Err(Error {
                        kind: ErrorKind::InvalidRequest,
                        reason: Some("source_kind_not_allowed"),
                    });
                }
                check_string(&source.id, 64, "invalid_reference")?;
            }
        }
        if let Some(state) = &self.state {
            if let Some(emotion) = &state.emotion {
                check_string(&emotion.description, 500, "emotion_too_long")?;
                check_optional(&emotion.target, 200, "emotion_too_long")?;
                check_optional(&emotion.reason, 500, "emotion_too_long")?;
                if emotion.signals.len() > 8 {
                    return Err(Error::invalid());
                }
                for signal in &emotion.signals {
                    check_string(&signal.label, 100, "signal_too_long")?;
                    if !(0.0..=1.0).contains(&signal.intensity) {
                        return Err(Error {
                            kind: ErrorKind::InvalidRequest,
                            reason: Some("signal_intensity_out_of_range"),
                        });
                    }
                }
            }
            if let Some(relationships) = &state.relationships {
                if relationships.len() > MAX_ITEMS {
                    return Err(Error::invalid());
                }
                for relationship in relationships {
                    check_string(&relationship.subject, 200, "relationship_too_long")?;
                    check_string(&relationship.understanding, 1000, "relationship_too_long")?;
                    if relationship.evidence.len() > MAX_ITEMS {
                        return Err(Error::invalid());
                    }
                    for id in &relationship.evidence {
                        check_string(id, 64, "invalid_reference")?;
                    }
                }
            }
            check_items(
                state.tendencies.as_deref().unwrap_or_default(),
                MAX_ITEMS,
                500,
                "tendency_too_long",
            )?;
            check_items(
                state.concerns.as_deref().unwrap_or_default(),
                MAX_ITEMS,
                500,
                "concern_too_long",
            )?;
        }
        Ok(())
    }

    /// The memory ids this request references, for the owner check.
    pub fn referenced_memory_ids(&self) -> impl Iterator<Item = &str> {
        self.memories
            .iter()
            .flat_map(|memory| {
                memory
                    .supersedes
                    .iter()
                    .chain(memory.sources.iter().map(|source| &source.id))
            })
            .chain(self.state.iter().flat_map(|state| {
                state
                    .relationships
                    .iter()
                    .flatten()
                    .flat_map(|relationship| relationship.evidence.iter())
            }))
            .map(String::as_str)
    }
}

impl ProfileDocument {
    pub fn validate(&self) -> Result<(), Error> {
        for items in [
            &self.facets,
            &self.examples,
            &self.background,
            &self.expression,
        ] {
            check_items(items, MAX_ITEMS, MAX_PROFILE_ITEM, "profile_too_long")?;
        }
        if serde_json::to_string(self)
            .map_err(|_| Error::invalid())?
            .len()
            > 16_000
        {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("profile_too_long"),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_mind_update_cannot_carry_a_profile_or_permission_change() {
        let update: std::result::Result<MindUpdateRequest, _> = serde_json::from_value(json!({
            "expected_revision": 1,
            "profile": {"core": "上書き"},
            "permissions": {"admin": true}
        }));
        assert!(update.is_err());
        let no_change: MindUpdateRequest = serde_json::from_value(json!({
            "expected_revision": 1
        }))
        .unwrap();
        assert!(no_change.is_empty());
    }
}
