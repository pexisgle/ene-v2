//! Host-bound mind tools. The model chooses what to recall or change; the host
//! binds the companion, operation id and input source and owns the store.

use crate::domain::mind::{MindUpdateRequest, SourceKind, SourceRef};
use bevy_ecs::prelude::{Component, Resource};
use rig_core::completion::{ToolAnnotations, UnsupportedOption};
use rig_core::effect::family;
use rig_core::effect::{EffectKind, HandlerDescriptor, HandlerKey};
use rig_core::serve::{Dispatch, ErasedHandler, Reply, Serve};
use rig_core::tool::{Tool, ToolContext, Touches};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug)]
pub(crate) enum MindCallKind {
    Recall,
    Read,
    Update,
}

/// Immutable facts the kernel binds to one tool call. The tool cannot choose
/// or change any of them; the host validates them again before reading or
/// writing the store.
#[derive(Clone, Debug)]
pub(crate) struct MindCallScope {
    pub companion: String,
    pub turn_start: String,
    pub call_id: String,
    pub operation_id: String,
    /// The user inputs this turn actually answers, including a Steer. Their
    /// ids and saved times come from the conversation, not from `now()`.
    pub sources: Vec<SourceRef>,
    pub allowed: bool,
}

impl MindCallScope {
    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

pub(crate) struct MindCall {
    pub kind: MindCallKind,
    pub scope: Arc<MindCallScope>,
    pub args: serde_json::Value,
    pub reply: oneshot::Sender<Result<String, String>>,
}

/// The kernel's handle to the host's mind operations. It owns no "current
/// companion"; each call carries its own immutable scope.
#[derive(Clone, Resource)]
pub(crate) struct MindBridge {
    calls: mpsc::UnboundedSender<MindCall>,
}

impl MindBridge {
    pub(crate) fn new(calls: mpsc::UnboundedSender<MindCall>) -> Self {
        Self { calls }
    }

    async fn call(
        &self,
        kind: MindCallKind,
        scope: Arc<MindCallScope>,
        args: serde_json::Value,
    ) -> Result<String, String> {
        if !scope.allowed {
            return Err("mind_not_permitted".into());
        }
        let (reply, answer) = oneshot::channel();
        self.calls
            .send(MindCall {
                kind,
                scope,
                args,
                reply,
            })
            .map_err(|_| "mind_unavailable".to_owned())?;
        answer.await.map_err(|_| "mind_unavailable".to_owned())?
    }
}

/// A tool call's handler with its per-call scope attached to the dispatch. The
/// tool reads the scope through `ToolContext::scope`, the public Rig mechanism
/// the kernel's dispatch provides.
pub(crate) struct BoundServe {
    pub inner: ErasedHandler,
    pub scope: Arc<MindCallScope>,
}

impl Serve for BoundServe {
    type Family = family::Tool;

    fn descriptor(&self) -> HandlerDescriptor {
        self.inner.descriptor()
    }

    fn key(&self) -> HandlerKey {
        self.inner.key().clone()
    }

    fn footprint(&self, kind: &EffectKind) -> Option<Touches> {
        self.inner.footprint(kind)
    }

    fn check(&self, kind: &EffectKind) -> Vec<UnsupportedOption> {
        self.inner.check(kind)
    }

    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        self.inner
            .handle(kind, dispatch.with_scope(self.scope.clone()))
            .await
    }
}

/// A call already bound to its scope, so the Prepare system wraps it once.
#[derive(Clone, Component)]
pub(crate) struct MindBound;

/// The stable id of one logical parent update: its companion, turn and the
/// provider's tool-call id. A retried tool execution keeps the same id even
/// when its result record was lost; no content matching is involved.
pub(crate) fn operation_id(companion: &str, turn_start: &str, call_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"ene.mind.update.v1\0");
    hash.update(companion.as_bytes());
    hash.update([0]);
    hash.update(turn_start.as_bytes());
    hash.update([0]);
    hash.update(call_id.as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The user inputs the turn answers, from its saved conversation entries. Tool
/// results are excluded; a Steer delivered into the turn is included; the
/// observed time is the entry's saved time.
pub(crate) fn turn_sources(
    conversation: &rig_core::conversation::Conversation,
    turn_start: rig_core::conversation::MessageId,
) -> Vec<SourceRef> {
    conversation
        .entries()
        .skip(conversation.start_of(turn_start))
        .filter(|entry| entry.is_users_own())
        .map(|entry| {
            let request = entry
                .author
                .and_then(|author| author.request.as_ref())
                .map(|request| request.0.clone());
            let observed_at =
                time::OffsetDateTime::from_unix_timestamp_nanos(entry.made as i128 * 1_000_000)
                    .ok()
                    .and_then(|time| {
                        time.format(&time::format_description::well_known::Rfc3339)
                            .ok()
                    })
                    .unwrap_or_else(|| entry.made.to_string());
            SourceRef {
                kind: SourceKind::Input,
                id: request.unwrap_or_else(|| entry.id.to_string()),
                entry_id: Some(entry.id.to_string()),
                speaker: Some("user".into()),
                observed_at: Some(observed_at),
            }
        })
        .collect()
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
pub(crate) struct MindRecall(pub(crate) Arc<MindBridge>);
#[derive(Clone)]
pub(crate) struct MindRead(pub(crate) Arc<MindBridge>);
#[derive(Clone)]
pub(crate) struct MindUpdate(pub(crate) Arc<MindBridge>);

fn mind_scope(context: &ToolContext) -> Result<Arc<MindCallScope>, MindToolError> {
    let scope = context
        .scope::<MindCallScope>()
        .ok_or_else(|| MindToolError("mind_context_missing".into()))?;
    if !scope.allowed {
        return Err(MindToolError("mind_not_permitted".into()));
    }
    Ok(scope)
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

    async fn call(
        &self,
        context: &mut ToolContext,
        args: RecallArgs,
    ) -> Result<String, MindToolError> {
        let scope = mind_scope(context)?;
        self.0
            .call(
                MindCallKind::Recall,
                scope,
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

    async fn call(
        &self,
        context: &mut ToolContext,
        args: ReadArgs,
    ) -> Result<String, MindToolError> {
        let scope = mind_scope(context)?;
        self.0
            .call(
                MindCallKind::Read,
                scope,
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
        context: &mut ToolContext,
        args: MindUpdateRequest,
    ) -> Result<String, MindToolError> {
        let scope = mind_scope(context)?;
        self.0
            .call(
                MindCallKind::Update,
                scope,
                serde_json::to_value(args).map_err(|error| MindToolError(error.to_string()))?,
            )
            .await
            .map_err(MindToolError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::conversation::{Author, AuthorKind, Conversation, Entry, RequestId};
    use rig_core::message::Message;

    fn author(request: &str) -> Author {
        Author {
            kind: AuthorKind::User,
            request: Some(RequestId(request.into())),
            ..Default::default()
        }
    }

    #[test]
    fn turn_sources_include_the_initial_and_steer_inputs_with_saved_times() {
        let mut conversation = Conversation::new();
        let start = conversation.push(Entry {
            made: Some(1_700_000_000_000),
            ..Entry::new(Message::user("最初")).by(author("request-1"))
        });
        conversation.push(Message::assistant("考え中"));
        conversation.push(Entry {
            made: Some(1_700_000_001_000),
            ..Entry::new(Message::user("追加")).by(author("request-2"))
        });
        let sources = turn_sources(&conversation, start);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].id, "request-1");
        assert_eq!(sources[1].id, "request-2");
        assert_eq!(sources[0].kind, SourceKind::Input);
        assert!(sources[0].entry_id.is_some() && sources[1].entry_id.is_some());
        assert!(!sources[0].observed_at.as_deref().unwrap().is_empty());
        assert!(!sources[1].observed_at.as_deref().unwrap().is_empty());
    }

    #[test]
    fn operation_ids_are_stable_per_call_and_distinct_across_calls() {
        let one = operation_id("companion-a", "turn-1", "call-1");
        assert_eq!(one, operation_id("companion-a", "turn-1", "call-1"));
        assert_ne!(one, operation_id("companion-b", "turn-1", "call-1"));
        assert_ne!(one, operation_id("companion-a", "turn-2", "call-1"));
        assert_ne!(one, operation_id("companion-a", "turn-1", "call-2"));
    }
}
