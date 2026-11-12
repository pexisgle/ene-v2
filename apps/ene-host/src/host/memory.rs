//! Episodic memory, its provenance and its visibility.
use serde_json::{Value, json};

pub(crate) const SHARED_AUDIENCE: &str = "*";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Acquisition {
    DirectExperience,
    UserStatement,
    Report,
    Shared,
    Inference,
}

impl Acquisition {
    pub(crate) const ALL: [Self; 5] = [
        Self::DirectExperience,
        Self::UserStatement,
        Self::Report,
        Self::Shared,
        Self::Inference,
    ];

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|acquisition| acquisition.as_str() == value)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DirectExperience => "direct_experience",
            Self::UserStatement => "user_statement",
            Self::Report => "report",
            Self::Shared => "shared",
            Self::Inference => "inference",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecallMode {
    /// Every-turn injection: forgotten memories are left out.
    Automatic,
    /// An explicit `mind_recall`: everything the viewer may see is eligible.
    Explicit,
}

#[derive(Clone, Debug)]
pub(crate) struct Memory {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) audiences: Vec<String>,
    pub(crate) content: String,
    pub(crate) description: String,
    pub(crate) appraisal: String,
    pub(crate) acquisition: Acquisition,
    pub(crate) importance: u8,
    pub(crate) occurred_at: Option<String>,
    pub(crate) recorded_at: String,
    pub(crate) last_accessed_at: Option<String>,
    pub(crate) memory_revision: u64,
    pub(crate) content_revision: u64,
    pub(crate) emotion: Option<String>,
    pub(crate) source_context: Value,
    pub(crate) supersedes: Vec<String>,
    pub(crate) created_by: String,
    pub(crate) updated_at: String,
}

impl Memory {
    pub(crate) fn visible_to(&self, viewer: &str) -> bool {
        self.owner == viewer
            || self
                .audiences
                .iter()
                .any(|audience| audience == viewer || audience == SHARED_AUDIENCE)
    }

    /// The memory as a tool or RPC client sees it.
    pub(crate) fn view_for(&self, viewer: &str) -> Value {
        if self.owner != viewer {
            return json!({
                "id": self.id,
                "content": self.content,
                "importance": self.importance,
                "occurred_at": self.occurred_at,
                "recorded_at": self.recorded_at,
            });
        }
        json!({
            "id": self.id,
            "owner": self.owner,
            "audiences": self.audiences,
            "content": self.content,
            "description": self.description,
            "appraisal": self.appraisal,
            "acquisition": self.acquisition.as_str(),
            "importance": self.importance,
            "occurred_at": self.occurred_at,
            "recorded_at": self.recorded_at,
            "last_accessed_at": self.last_accessed_at,
            "revision": self.memory_revision,
            "content_revision": self.content_revision,
            "emotion": self.emotion,
            "sources": self.source_context,
            "supersedes": self.supersedes,
            "created_by": self.created_by,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryAction {
    Forget,
    Remember,
    Share,
    Unshare,
}

impl MemoryAction {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "forget" => Some(Self::Forget),
            "remember" => Some(Self::Remember),
            "share" => Some(Self::Share),
            "unshare" => Some(Self::Unshare),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MindOperation {
    pub(crate) operation_id: String,
    pub(crate) hash: String,
    pub(crate) response: String,
}
