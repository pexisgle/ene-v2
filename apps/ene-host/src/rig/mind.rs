//! Host-bound mind tools. The model chooses what to recall or change; the host
//! binds the companion, operation id and input source and owns the store.

use crate::domain::mind::MindUpdateRequest;
use bevy_ecs::prelude::Resource;
use rig_core::completion::ToolAnnotations;
use rig_core::tool::{Tool, ToolContext};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug)]
pub(crate) enum MindCallKind {
    Recall,
    Read,
    Update,
}

pub(crate) struct MindCall {
    pub kind: MindCallKind,
    pub companion: String,
    pub args: serde_json::Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// The kernel's handle to the host's mind operations. It lives in the app as a
/// resource and in each mind tool; both share the binding.
#[derive(Clone, Resource)]
pub(crate) struct MindBridge {
    companion: Arc<Mutex<Option<String>>>,
    calls: mpsc::UnboundedSender<MindCall>,
}

impl MindBridge {
    pub(crate) fn new(calls: mpsc::UnboundedSender<MindCall>) -> Self {
        Self {
            companion: Arc::new(Mutex::new(None)),
            calls,
        }
    }

    pub(crate) fn bind(&self, companion: String) {
        *self
            .companion
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(companion);
    }

    pub(crate) fn bound(&self) -> bool {
        self.companion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    async fn call(&self, kind: MindCallKind, args: serde_json::Value) -> Result<String, String> {
        let companion = self
            .companion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or_else(|| "mind_not_bound".to_owned())?;
        let (reply, answer) = oneshot::channel();
        self.calls
            .send(MindCall {
                kind,
                companion,
                args,
                reply,
            })
            .map_err(|_| "mind_unavailable".to_owned())?;
        answer.await.map_err(|_| "mind_unavailable".to_owned())?
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct MindToolError(String);

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecallArgs {
    pub query: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadArgs {
    pub id: String,
}

#[derive(Clone)]
pub(crate) struct MindRecall(Arc<MindBridge>);
#[derive(Clone)]
pub(crate) struct MindRead(Arc<MindBridge>);
#[derive(Clone)]
pub(crate) struct MindUpdate(Arc<MindBridge>);

impl MindRecall {
    pub(crate) fn new(bridge: Arc<MindBridge>) -> Self {
        Self(bridge)
    }
}
impl MindRead {
    pub(crate) fn new(bridge: Arc<MindBridge>) -> Self {
        Self(bridge)
    }
}
impl MindUpdate {
    pub(crate) fn new(bridge: Arc<MindBridge>) -> Self {
        Self(bridge)
    }
}

impl Tool for MindRecall {
    const NAME: &'static str = "mind_recall";
    type Args = RecallArgs;
    type Output = String;
    type Error = MindToolError;

    fn description(&self) -> String {
        "Search this companion's saved memories by words. Returns matching memories with ids, content, meaning, provenance and correction links. Use it when a past preference, agreement, experience or correction may matter; do not search every turn.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": 200},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 8}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_read_only(true)
    }

    async fn call(&self, _: &mut ToolContext, args: RecallArgs) -> Result<String, MindToolError> {
        self.0
            .call(
                MindCallKind::Recall,
                serde_json::to_value(args).map_err(|error| MindToolError(error.to_string()))?,
            )
            .await
            .map_err(MindToolError)
    }
}

impl Tool for MindRead {
    const NAME: &'static str = "mind_read";
    type Args = ReadArgs;
    type Output = String;
    type Error = MindToolError;

    fn description(&self) -> String {
        "Read one saved memory in full by id, including its provenance and which memories correct it. Use it before correcting a memory or when recall gave only a summary.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"id": {"type": "string"}},
            "required": ["id"],
            "additionalProperties": false
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::default().with_read_only(true)
    }

    async fn call(&self, _: &mut ToolContext, args: ReadArgs) -> Result<String, MindToolError> {
        self.0
            .call(
                MindCallKind::Read,
                serde_json::to_value(args).map_err(|error| MindToolError(error.to_string()))?,
            )
            .await
            .map_err(MindToolError)
    }
}

impl Tool for MindUpdate {
    const NAME: &'static str = "mind_update";
    type Args = MindUpdateRequest;
    type Output = String;
    type Error = MindToolError;

    fn description(&self) -> String {
        "Store a meaningful experience, preference, agreement or correction and/or update the current mind. The owner, operation id and input source are bound by the host; the author profile and permissions cannot be changed. Use expected_revision from the current mind in the prompt. A correction keeps the old memory and cites it in supersedes. A no-change request is a normal success. Do not call this every turn, and do not treat author examples or your own guesses as observed experience.".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expected_revision": {"type": "integer", "minimum": 0},
                "memories": {"type": "array", "maxItems": 16, "items": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "maxLength": 4000},
                        "description": {"type": "string", "maxLength": 1000},
                        "appraisal": {"type": "string", "maxLength": 1000},
                        "acquisition": {"type": "string", "enum": ["direct_experience", "user_statement", "report", "shared", "inference"]},
                        "supersedes": {"type": "array", "maxItems": 16, "items": {"type": "string"}},
                        "sources": {"type": "array", "maxItems": 16, "items": {
                            "type": "object",
                            "properties": {"kind": {"type": "string", "enum": ["memory"]}, "id": {"type": "string"}},
                            "required": ["kind", "id"], "additionalProperties": false
                        }},
                        "occurred_at": {"type": "string", "maxLength": 64}
                    },
                    "required": ["content", "acquisition"],
                    "additionalProperties": false
                }},
                "state": {
                    "type": "object",
                    "properties": {
                        "emotion": {"type": "object", "properties": {
                            "description": {"type": "string", "maxLength": 500},
                            "target": {"type": "string", "maxLength": 200},
                            "reason": {"type": "string", "maxLength": 500},
                            "signals": {"type": "array", "maxItems": 8, "items": {"type": "object", "properties": {
                                "label": {"type": "string", "maxLength": 100},
                                "intensity": {"type": "number", "minimum": 0, "maximum": 1}
                            }, "required": ["label", "intensity"], "additionalProperties": false}}
                        }, "required": ["description"], "additionalProperties": false},
                        "relationships": {"type": "array", "maxItems": 16, "items": {"type": "object", "properties": {
                            "subject": {"type": "string", "maxLength": 200},
                            "understanding": {"type": "string", "maxLength": 1000},
                            "evidence": {"type": "array", "maxItems": 16, "items": {"type": "string"}}
                        }, "required": ["subject", "understanding"], "additionalProperties": false}},
                        "tendencies": {"type": "array", "maxItems": 16, "items": {"type": "string", "maxLength": 500}},
                        "concerns": {"type": "array", "maxItems": 16, "items": {"type": "string", "maxLength": 500}}
                    },
                    "additionalProperties": false
                }
            },
            "required": ["expected_revision"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _: &mut ToolContext,
        args: MindUpdateRequest,
    ) -> Result<String, MindToolError> {
        self.0
            .call(
                MindCallKind::Update,
                serde_json::to_value(args).map_err(|error| MindToolError(error.to_string()))?,
            )
            .await
            .map_err(MindToolError)
    }
}
