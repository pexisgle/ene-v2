//! The parent's mind: author profile and current state in the model's input,
//! and the host-bound recall/read/update path.

use super::{Boundary, HostApplication, generation::publish};
use crate::rig::mind::{MindCall, MindCallKind, ReadArgs, RecallArgs};
use crate::{
    domain::{
        Error, ErrorKind,
        mind::{
            MemoryRecord, MindUpdateRequest, MindUpdateResult, ProfileDocument, SourceKind,
            SourceRef, StoredMind,
        },
    },
    ports::Result,
};
use ene_rpc as w;
use serde_json::{Value, json};

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn document(value: &Value) -> w::JsonDocument {
    w::JsonDocument {
        json: serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
        ..Default::default()
    }
}

/// The wire view of the authored profile and current state. `core` stays on
/// the companion; the profile table holds the author's extra material.
fn view(companion: &str, core: &str, stored: StoredMind) -> w::MindView {
    let profile = w::MindProfile {
        core: core.into(),
        revision: stored.profile_revision,
        facets: stored.profile.facets,
        examples: stored.profile.examples,
        background: stored.profile.background,
        expression: stored.profile.expression,
        ..Default::default()
    };
    let state = w::MindState {
        revision: stored.state_revision,
        emotion: document(&stored.state.emotion.clone().unwrap_or_else(|| json!({}))).into(),
        relationships: document(&Value::Array(stored.state.relationships.clone())).into(),
        tendencies: document(&json!(stored.state.tendencies)).into(),
        concerns: document(&json!(stored.state.concerns)).into(),
        updated_at: stored.updated_at,
        ..Default::default()
    };
    w::MindView {
        companion_id: companion.into(),
        profile: profile.into(),
        state: state.into(),
        ..Default::default()
    }
}

/// What the model needs to know: authored material, the current mind, and how
/// to use the three host tools. This is a view; SQLite stays canonical.
pub(crate) fn compose_prompt(core: &str, stored: &StoredMind) -> String {
    let mut out = String::with_capacity(12_000);
    out.push_str(
        "あなたは次の人格設定を持つコンパニオンとして、この会話の相手と話す。\n\
         作者設定はロールプレイの前提であり、あなたが相手と実際に経験したことではない。\n\n\
         [作者設定]\n核:\n",
    );
    out.push_str(core.trim());
    out.push('\n');
    for (label, items) in [
        ("側面", &stored.profile.facets),
        ("背景", &stored.profile.background),
        ("表現の語彙", &stored.profile.expression),
    ] {
        if !items.is_empty() {
            out.push_str(label);
            out.push_str(":\n");
            for item in items {
                out.push_str("- ");
                out.push_str(item.trim());
                out.push('\n');
            }
        }
    }
    if !stored.profile.examples.is_empty() {
        out.push_str(
            "会話・行動の例（固定の台詞ではなく、話し方と態度の材料。実体験として記憶しない）:\n",
        );
        for example in &stored.profile.examples {
            out.push_str("- ");
            out.push_str(example.trim());
            out.push('\n');
        }
    }
    out.push_str("\n[現在の心]\n");
    out.push_str(&format!(
        "revision: {}（この値は mind_update の expected_revision に使う）\n",
        stored.state_revision
    ));
    render_current_state(&mut out, &stored.state);
    out.push_str(
        "\n[心と記憶の操作]\n\
         - mind_recall: 保存済みの記憶を語句で検索する。必要なときだけ使う。\n\
         - mind_read: 記憶の全文・出所・訂正関係を読む。\n\
         - mind_update: 相手との経験・明示された好み・合意・訂正、または現在の感情・関係・傾向を構造化して保存する。\
         変化が意味を持つときだけ使い、毎回は使わない。expected_revision は上記の現在値。\
         訂正では、まず mind_recall / mind_read で旧い記憶の id を確かめ、新しい記憶の supersedes に入れる。\
         作者の例や、権限・作者設定の変更は保存できない。\n\
         想起しただけ、同じ出来事を言い直しただけで関係性を加点しない。\n",
    );
    out
}

fn render_current_state(out: &mut String, state: &crate::domain::mind::StoredState) {
    match &state.emotion {
        Some(emotion) if emotion.as_object().is_some_and(|object| !object.is_empty()) => {
            let description = emotion
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.push_str("現在の感情: ");
            out.push_str(description);
            if let Some(target) = emotion.get("target").and_then(Value::as_str) {
                out.push_str("（対象: ");
                out.push_str(target);
                out.push('）');
            }
            if let Some(reason) = emotion.get("reason").and_then(Value::as_str) {
                out.push_str("（理由: ");
                out.push_str(reason);
                out.push('）');
            }
            if let Some(signals) = emotion.get("signals").and_then(Value::as_array) {
                let rendered = signals
                    .iter()
                    .filter_map(|signal| {
                        Some(format!(
                            "{} {:.2}",
                            signal.get("label")?.as_str()?,
                            signal.get("intensity")?.as_f64()?
                        ))
                    })
                    .collect::<Vec<_>>();
                if !rendered.is_empty() {
                    out.push_str("（表現: ");
                    out.push_str(&rendered.join(", "));
                    out.push('）');
                }
            }
            out.push('\n');
        }
        _ => out.push_str("現在の感情: 特になし\n"),
    }
    if state.relationships.is_empty() {
        out.push_str("関係の理解: 特になし\n");
    } else {
        out.push_str("関係の理解:\n");
        for relationship in &state.relationships {
            let subject = relationship
                .get("subject")
                .and_then(Value::as_str)
                .unwrap_or("相手");
            let understanding = relationship
                .get("understanding")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.push_str("- ");
            out.push_str(subject);
            out.push_str(": ");
            out.push_str(understanding);
            out.push('\n');
        }
    }
    for (label, items) in [
        ("経験してきた傾向", &state.tendencies),
        ("気にかけていること", &state.concerns),
    ] {
        if !items.is_empty() {
            out.push_str(label);
            out.push_str(":\n");
            for item in items {
                out.push_str("- ");
                out.push_str(item);
                out.push('\n');
            }
        }
    }
}

impl HostApplication {
    pub(crate) async fn get_mind(&self, companion: String) -> Result<w::MindView> {
        let c = self.companion(&companion).await?;
        let stored = self
            .0
            .repository
            .mind_view(companion.clone(), now())
            .await?;
        Ok(view(&companion, &c.mind, stored))
    }

    pub(crate) async fn update_mind_profile(
        &self,
        request: w::UpdateMindProfileRequest,
    ) -> Result<w::MindView> {
        let companion = request.companion_id;
        crate::domain::CompanionId::parse(companion.clone())?;
        let profile = ProfileDocument {
            facets: request.facets,
            examples: request.examples,
            background: request.background,
            expression: request.expression,
        };
        profile.validate()?;
        if profile.examples.len() > 16 || profile.facets.len() > 16 {
            return Err(Error::invalid());
        }
        let c = self.companion(&companion).await?;
        let (_, _) = self
            .0
            .repository
            .mind_update_profile(companion.clone(), request.expected_revision, profile, now())
            .await?;
        let stored = self
            .0
            .repository
            .mind_view(companion.clone(), now())
            .await?;
        publish_mind(self, &companion, &c.mind, stored).await
    }

    /// Commits the parent's structured update, then updates the model input and
    /// notifies clients. The tool caller never chooses the owner, operation id
    /// or input source.
    pub(crate) async fn apply_mind_update(
        &self,
        companion: String,
        operation_id: String,
        request: MindUpdateRequest,
    ) -> Result<MindUpdateResult> {
        request.validate()?;
        let source = {
            let controls = self.0.controls.lock().await;
            controls.mind_inputs.get(&companion).cloned()
        };
        let Some(source) = source else {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("source_unavailable"),
            });
        };
        let c = self.companion(&companion).await?;
        let request_hash = serde_json::to_string(&(&companion, &request, &source))
            .map_err(|_| Error::invalid())?;
        let result = self
            .0
            .repository
            .mind_update(
                companion.clone(),
                operation_id,
                request_hash,
                request,
                source,
                now(),
            )
            .await?;
        if result.changed {
            let stored = self
                .0
                .repository
                .mind_view(companion.clone(), now())
                .await?;
            publish_mind(self, &companion, &c.mind, stored).await?;
        }
        Ok(result)
    }

    pub(crate) async fn mind_recall(
        &self,
        companion: String,
        query: String,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        if query.trim().is_empty() || query.chars().count() > 200 || !(1..=20).contains(&limit) {
            return Err(Error::invalid());
        }
        self.companion(&companion).await?;
        self.0.repository.mind_recall(companion, query, limit).await
    }

    pub(crate) async fn mind_read(
        &self,
        companion: String,
        id: String,
    ) -> Result<Option<MemoryRecord>> {
        uuid::Uuid::parse_str(&id).map_err(|_| Error::invalid())?;
        self.companion(&companion).await?;
        self.0.repository.mind_read(companion, id).await
    }

    /// The prompt composed at reconnect time from the current stored mind.
    pub(crate) async fn mind_prompt(&self, companion: &str, core: &str) -> Result<String> {
        let stored = self
            .0
            .repository
            .mind_view(companion.to_owned(), now())
            .await?;
        Ok(compose_prompt(core, &stored))
    }
}

/// Publish the committed state and refresh the live model input. Both happen
/// only after the SQLite commit; a failure here does not roll the mind back.
async fn publish_mind(
    app: &HostApplication,
    companion: &str,
    core: &str,
    stored: StoredMind,
) -> Result<w::MindView> {
    let view = view(companion, core, stored.clone());
    if let Ok(runtime) = app.rig() {
        let prompt = compose_prompt(core, &stored);
        if let Err(error) = runtime.set_mind_prompt(companion.to_owned(), prompt) {
            // The next connect composes from SQLite; the update is not rolled
            // back for a missing kernel.
            let _ = error;
        }
    }
    let mut boundary: tokio::sync::MutexGuard<'_, Boundary> = app.0.boundary.lock().await;
    publish(
        &mut boundary,
        app,
        Some(companion),
        ene_rpc::notification::Event::MindUpdated(Box::new(view.clone())),
    );
    Ok(view)
}

impl HostApplication {
    /// Runs one host-bound mind tool call and replies to the kernel. The model
    /// supplies the content; the host supplies owner, operation id and source.
    pub(crate) async fn handle_mind_call(&self, call: MindCall) {
        let result = match call.kind {
            MindCallKind::Recall => match serde_json::from_value::<RecallArgs>(call.args) {
                Ok(args) => match self
                    .mind_recall(
                        call.companion,
                        args.query,
                        args.limit.unwrap_or(8).clamp(1, 20) as usize,
                    )
                    .await
                {
                    Ok(memories) => Ok(json!({
                        "count": memories.len(),
                        "memories": memories,
                    })
                    .to_string()),
                    Err(error) => Err(tool_error(error)),
                },
                Err(_) => Err(tool_error(Error::invalid())),
            },
            MindCallKind::Read => match serde_json::from_value::<ReadArgs>(call.args) {
                Ok(args) => match self.mind_read(call.companion, args.id).await {
                    Ok(Some(memory)) => Ok(json!({"found": true, "memory": memory}).to_string()),
                    Ok(None) => Ok(json!({"found": false}).to_string()),
                    Err(error) => Err(tool_error(error)),
                },
                Err(_) => Err(tool_error(Error::invalid())),
            },
            MindCallKind::Update => match serde_json::from_value(call.args) {
                Ok(request) => match self
                    .apply_mind_update(call.companion, uuid::Uuid::new_v4().to_string(), request)
                    .await
                {
                    Ok(result) => Ok(serde_json::to_string(&result).unwrap_or_default()),
                    Err(error) => Err(tool_error(error)),
                },
                Err(_) => Err(tool_error(Error::invalid())),
            },
        };
        let _ = call.reply.send(result);
    }
}

fn tool_error(error: Error) -> String {
    let code = match error.kind {
        ErrorKind::RevisionConflict => "revision_conflict",
        ErrorKind::IdempotencyConflict => "idempotency_conflict",
        ErrorKind::InvalidRequest => error.reason.unwrap_or("invalid_request"),
        ErrorKind::NotFound => "not_found",
        ErrorKind::StorageFailure => "storage_failure",
        _ => "unavailable",
    };
    json!({"error": code}).to_string()
}

/// Keeps the host's end of the mind tool channel for the kernel's lifetime.
pub(crate) fn spawn_mind_tools(
    app: HostApplication,
    mut calls: tokio::sync::mpsc::UnboundedReceiver<MindCall>,
) {
    tokio::spawn(async move {
        while let Some(call) = calls.recv().await {
            app.handle_mind_call(call).await;
        }
    });
}

/// The source a saved memory cites: the accepted input of the current turn.
pub(crate) fn input_source(stream_id: &str, request_id: &str) -> SourceRef {
    SourceRef {
        kind: SourceKind::Input,
        id: request_id.into(),
        entry_id: Some(stream_id.into()),
        speaker: Some("user".into()),
        observed_at: Some(now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mind::{ProfileDocument, StoredMind, StoredState};

    #[test]
    fn the_prompt_separates_authored_material_from_lived_state() {
        let stored = StoredMind {
            profile_revision: 2,
            profile: ProfileDocument {
                facets: vec!["静かだがよく見ている".into()],
                examples: vec!["「なるほど」と短く返す".into()],
                ..Default::default()
            },
            state_revision: 3,
            state: StoredState {
                emotion: Some(json!({
                    "description": "少しうれしい",
                    "target": "ユーザー",
                    "signals": [{"label":"声がやわらぐ","intensity":0.4}]
                })),
                relationships: vec![
                    json!({"subject":"ユーザー","understanding":"好みを話してくれる"}),
                ],
                tendencies: vec!["約束を覚えている".into()],
                concerns: vec!["無理をしていないか".into()],
            },
            updated_at: "2026-01-01T00:00:00Z".into(),
        };
        let prompt = compose_prompt("日本語で答える。", &stored);
        assert!(prompt.contains("日本語で答える。"));
        assert!(prompt.contains("実体験として記憶しない"));
        assert!(prompt.contains("少しうれしい"));
        assert!(prompt.contains("revision: 3"));
        assert!(prompt.contains("mind_update"));
    }
}
