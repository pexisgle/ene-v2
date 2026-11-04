//! The parent's mind: author profile and current state in the model's input,
//! and the host-bound recall/read/update path.

use super::{Boundary, HostApplication, generation::publish};
use crate::application::inference::EMBEDDING_MODEL;
use crate::rig::mind::{MindCall, MindCallKind, ReadArgs, RecallArgs};
use crate::{
    domain::{
        Error, ErrorKind,
        mind::{MemoryRecord, MindUpdateRequest, MindUpdateResult, ProfileDocument, StoredMind},
    },
    ports::Result,
};
use ene_rpc as w;
use serde_json::{Value, json};

pub(crate) fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn compose_recall_fragment(
    companion: &str,
    recalled: &[MemoryRecord],
    lexical: &str,
    semantic: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[いまの話題で思い出した記憶]\n検索状態: 語句={lexical} 意味={semantic}\n"
    ));
    if recalled.is_empty() {
        out.push_str("該当する記憶はない。想起できていない場合と混同しないため、必要なら mind_recall を使う。\n");
    } else {
        for memory in recalled.iter().take(3) {
            let shared = if memory.owner != companion {
                "（共有された記憶）"
            } else {
                ""
            };
            out.push_str(&format!(
                "- id: {} {} revision {}\n  {}\n",
                memory.id,
                shared,
                memory.revision,
                memory.content.chars().take(600).collect::<String>()
            ));
            if !memory.description.is_empty() {
                out.push_str(&format!(
                    "  説明: {}\n",
                    memory.description.chars().take(300).collect::<String>()
                ));
            }
        }
    }
    out
}

/// The wire view of the authored profile and current state. `core` stays on
/// the companion; the profile table holds the author's extra material.
fn view(
    companion: &str,
    core: &str,
    stored: StoredMind,
    index: crate::infrastructure::sqlite::memory::IndexStatus,
    consolidation: crate::infrastructure::sqlite::memory::ConsolidationStatus,
) -> w::MindView {
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
        emotion: w::JsonDocument::from(&stored.state.emotion.unwrap_or_else(|| json!({}))).into(),
        relationships: w::JsonDocument::from(&Value::Array(stored.state.relationships)).into(),
        tendencies: w::JsonDocument::from(&json!(stored.state.tendencies)).into(),
        concerns: w::JsonDocument::from(&json!(stored.state.concerns)).into(),
        updated_at: stored.updated_at,
        ..Default::default()
    };
    w::MindView {
        companion_id: companion.into(),
        profile: profile.into(),
        state: state.into(),
        index: w::MindIndexStatus {
            state: index.state,
            reason: index.reason,
            pending_fts: index.pending_fts,
            pending_vectors: index.pending_vectors,
            embedding_model: index.embedding_model,
            embedding_calls: index.embedding_calls,
            embedding_tokens: index.embedding_tokens,
            updated_at: index.updated_at,
            ..Default::default()
        }
        .into(),
        consolidation: w::MindConsolidationStatus {
            last_sequence: consolidation.last_sequence,
            last_offset: consolidation.last_offset,
            method_revision: consolidation.method_revision,
            state: consolidation.state,
            reason: consolidation.reason,
            jobs: consolidation.jobs,
            skipped: consolidation.skipped,
            model_calls: consolidation.model_calls,
            input_chars: consolidation.input_chars,
            updated_at: consolidation.updated_at,
            ..Default::default()
        }
        .into(),
        ..Default::default()
    }
}

#[derive(Clone)]
pub(crate) struct ActiveRecall {
    pub request_id: String,
    pub memories: Vec<MemoryRecord>,
    pub lexical: String,
    pub reason: Option<String>,
}

pub(crate) struct RecallResult {
    pub memories: Vec<MemoryRecord>,
    pub lexical: String,
    pub semantic: String,
    pub pending: u64,
    pub reason: Option<String>,
}

/// What the model needs to know: authored material, the current mind, and how
/// to use the three host tools. This is a view; SQLite stays canonical.
pub(crate) fn compose_prompt(core: &str, stored: &StoredMind) -> String {
    compose_prompt_with("", core, stored, &[], "unchecked", "unchecked")
}

pub(crate) fn compose_prompt_with(
    companion: &str,
    core: &str,
    stored: &StoredMind,
    recalled: &[MemoryRecord],
    lexical: &str,
    semantic: &str,
) -> String {
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
    out.push_str(&format!(
        "\n[いまの話題で思い出した記憶]\n検索状態: 語句={lexical} 意味={semantic}\n"
    ));
    if recalled.is_empty() {
        out.push_str("該当する記憶はない。想起できていない場合と混同しないため、必要なら mind_recall を使う。\n");
    } else {
        for memory in recalled.iter().take(3) {
            let shared = if memory.owner != companion {
                "（共有された記憶）"
            } else {
                ""
            };
            out.push_str(&format!(
                "- id: {} {} revision {}\n  {}\n",
                memory.id,
                shared,
                memory.revision,
                memory.content.chars().take(600).collect::<String>()
            ));
            if !memory.description.is_empty() {
                out.push_str(&format!(
                    "  説明: {}\n",
                    memory.description.chars().take(300).collect::<String>()
                ));
            }
        }
    }
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
        let index = self
            .0
            .repository
            .mind_index_status(companion.clone(), now())
            .await?;
        let consolidation = self
            .0
            .repository
            .mind_consolidation(
                companion.clone(),
                crate::application::memory::METHOD_REVISION.into(),
                now(),
            )
            .await?;
        Ok(view(&companion, &c.mind, stored, index, consolidation))
    }

    pub(crate) async fn update_mind_profile(
        &self,
        request: w::UpdateMindProfileRequest,
    ) -> Result<w::MindView> {
        let companion = request.companion_id;
        crate::domain::validate_id(&companion)?;
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
        self.notify_mind_work();
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
        sources: Vec<crate::domain::mind::SourceRef>,
    ) -> Result<MindUpdateResult> {
        request.validate()?;
        if operation_id.is_empty()
            || sources.is_empty()
            || sources.iter().any(|source| {
                source.kind != crate::domain::mind::SourceKind::Input
                    || source.id.is_empty()
                    || source.observed_at.as_deref().is_none_or(str::is_empty)
            })
        {
            return Err(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("source_unavailable"),
            });
        }
        let c = self.companion(&companion).await?;
        let request_hash = serde_json::to_string(&(&companion, &request, &sources))
            .map_err(|_| Error::invalid())?;
        let result = self
            .0
            .repository
            .mind_update(
                companion.clone(),
                operation_id,
                request_hash,
                request,
                sources,
                "parent".into(),
                now(),
            )
            .await?;
        if result.changed {
            self.notify_mind_work();
            let stored = self
                .0
                .repository
                .mind_view(companion.clone(), now())
                .await?;
            publish_mind(self, &companion, &c.mind, stored).await?;
        }
        Ok(result)
    }

    pub(crate) async fn control_memory(
        &self,
        companion: String,
        memory_id: String,
        action: String,
    ) -> Result<MemoryRecord> {
        crate::domain::validate_id(&companion)?;
        uuid::Uuid::parse_str(&memory_id).map_err(|_| Error::invalid())?;
        self.companion(&companion).await?;
        self.0
            .repository
            .mind_control(companion, memory_id, action, now())
            .await
    }

    async fn mind_recall_lexical(
        &self,
        companion: String,
        mut query: String,
        limit: usize,
    ) -> Result<RecallResult> {
        // The automatic path is local SQLite only. It must not wait on an
        // embedding provider before the input can be delivered. The query is
        // bounded explicitly and the truncation is visible in the result.
        let mut reason = None;
        let length = query.chars().count();
        if length > 4000 {
            query = query.chars().take(4000).collect();
            reason = Some("query_truncated".to_owned());
        }
        if query.trim().is_empty() {
            return Err(Error::invalid());
        }
        self.companion(&companion).await?;
        let result = self
            .0
            .repository
            .mind_search(
                companion,
                query,
                None,
                None,
                limit,
                false,
                None,
                None,
                now(),
            )
            .await?;
        Ok(RecallResult {
            memories: result.memories,
            lexical: result.fts.to_owned(),
            semantic: "not_requested".into(),
            pending: result.pending,
            reason: reason.or(result.reason),
        })
    }

    fn set_active_request(&self, companion: &str, request: &str) {
        if let Ok(mut active) = self.0.active_request.lock() {
            active.insert(companion.to_owned(), request.to_owned());
        }
    }

    fn active_request(&self, companion: &str) -> Option<String> {
        self.0
            .active_request
            .lock()
            .ok()
            .and_then(|active| active.get(companion).cloned())
    }

    fn set_active_recall(&self, companion: &str, recall: ActiveRecall) {
        if let Ok(mut cache) = self.0.active_recall.lock() {
            cache.insert(companion.to_owned(), recall);
        }
    }

    pub(crate) fn clear_active_recall(&self, companion: &str, request_id: &str) {
        if let Ok(mut cache) = self.0.active_recall.lock()
            && cache
                .get(companion)
                .is_some_and(|recall| recall.request_id == request_id)
        {
            cache.remove(companion);
        }
        if let Ok(mut active) = self.0.active_request.lock()
            && active
                .get(companion)
                .is_some_and(|active| active == request_id)
        {
            active.remove(companion);
        }
        self.cancel_semantic(companion, request_id);
    }

    fn active_recall(&self, companion: &str) -> Option<ActiveRecall> {
        self.0
            .active_recall
            .lock()
            .ok()
            .and_then(|cache| cache.get(companion).cloned())
    }

    pub(crate) async fn mind_recall(
        &self,
        companion: String,
        query: String,
        limit: usize,
        include_forgotten: bool,
        since: Option<String>,
        until: Option<String>,
    ) -> Result<RecallResult> {
        if query.trim().is_empty()
            || query.chars().count() > 2000
            || !(1..=20).contains(&limit)
            || since.as_deref().is_some_and(|value| value.len() > 64)
            || until.as_deref().is_some_and(|value| value.len() > 64)
        {
            return Err(Error::invalid());
        }
        self.companion(&companion).await?;
        let embedded = self.embed_memory(&companion, vec![query.clone()]).await;
        let (vector, model, semantic, reason) = match embedded {
            Ok(embedding) if embedding.vectors.len() == 1 => (
                Some(embedding.vectors.into_iter().next().unwrap_or_default()),
                Some(EMBEDDING_MODEL.to_owned()),
                "ok".to_owned(),
                None,
            ),
            Ok(_) => (
                None,
                None,
                "error".to_owned(),
                Some("embedding_shape".to_owned()),
            ),
            Err(error) => (
                None,
                None,
                match error.kind {
                    ErrorKind::UnsupportedCapability | ErrorKind::StateConflict => {
                        "unavailable".to_owned()
                    }
                    _ => "error".to_owned(),
                },
                error.reason.map(str::to_owned),
            ),
        };
        let result = self
            .0
            .repository
            .mind_search(
                companion,
                query,
                vector,
                model,
                limit,
                include_forgotten,
                since,
                until,
                now(),
            )
            .await?;
        Ok(RecallResult {
            memories: result.memories,
            lexical: result.fts.to_owned(),
            semantic,
            pending: result.pending,
            reason: reason.or(result.reason),
        })
    }

    pub(crate) async fn mind_read(
        &self,
        companion: String,
        id: String,
    ) -> Result<Option<MemoryRecord>> {
        uuid::Uuid::parse_str(&id).map_err(|_| Error::invalid())?;
        self.companion(&companion).await?;
        self.0.repository.mind_read_visible(companion, id).await
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

    /// The prompt for an accepted input: current mind plus a small automatic
    /// recall. A recall or embedding failure does not fail the turn; the status
    /// in the prompt says what was not available.
    pub(crate) async fn mind_prompt_with_recall(
        &self,
        companion: &str,
        core: &str,
        text: &str,
        request_id: &str,
    ) -> Result<String> {
        let recall = self
            .mind_recall_lexical(companion.to_owned(), text.to_owned(), 3)
            .await;
        let (memories, lexical, reason) = match recall {
            Ok(result) => (result.memories, result.lexical, result.reason),
            Err(_) => (
                Vec::new(),
                "error".to_owned(),
                Some("lexical_error".to_owned()),
            ),
        };
        let stored = self
            .0
            .repository
            .mind_view(companion.to_owned(), now())
            .await?;
        let lexical_status = match &reason {
            Some(reason) => format!("{lexical}({reason})"),
            None => lexical.clone(),
        };
        let prompt = compose_prompt_with(
            companion,
            core,
            &stored,
            &memories,
            &lexical_status,
            "lexical_only",
        );
        self.set_active_request(companion, request_id);
        let sequence = self.next_prompt_sequence();
        if let Ok(runtime) = self.rig() {
            let _ = runtime.set_mind_prompt(companion.to_owned(), prompt.clone(), sequence);
            let _ = runtime.hold_recall(
                companion.to_owned(),
                request_id.to_owned(),
                std::time::Instant::now() + std::time::Duration::from_millis(900),
            );
        }
        let local = RecallResult {
            memories: memories.clone(),
            lexical: lexical.clone(),
            semantic: "lexical_only".into(),
            pending: 0,
            reason: reason.clone(),
        };
        let app = self.clone();
        let companion_owned = companion.to_owned();
        let request_owned = request_id.to_owned();
        let text_owned = text.to_owned();
        let core_owned = core.to_owned();
        let local_prompt = prompt.clone();
        self.0.background.spawn(async move {
            app.semantic_recall(
                companion_owned,
                core_owned,
                text_owned,
                request_owned,
                local,
                local_prompt,
            )
            .await;
        });
        self.set_active_recall(
            companion,
            ActiveRecall {
                request_id: request_id.to_owned(),
                memories,
                lexical,
                reason,
            },
        );
        Ok(prompt)
    }

    /// Cancel any hold and drop the per-input search state without touching
    /// the canonical mind.
    fn cancel_semantic(&self, companion: &str, request: &str) {
        if let Ok(runtime) = self.rig() {
            let _ = runtime.cancel_recall(request.to_owned());
        }
        if self.active_request(companion).as_deref() == Some(request)
            && let Ok(mut active) = self.0.active_request.lock()
        {
            active.remove(companion);
        }
    }

    async fn semantic_recall(
        &self,
        companion: String,
        core: String,
        text: String,
        request_id: String,
        local: RecallResult,
        local_prompt: String,
    ) {
        let finish = |prompt: String| {
            if let Ok(runtime) = self.rig() {
                let _ = runtime.complete_recall(request_id.clone(), prompt);
            }
        };
        let cancel = |app: &HostApplication| app.cancel_semantic(&companion, &request_id);
        if self.active_request(&companion).as_deref() != Some(request_id.as_str()) {
            cancel(self);
            return;
        }
        let count = self
            .0
            .repository
            .mind_memory_count(companion.clone())
            .await
            .unwrap_or(0);
        if count == 0 || text.chars().count() < 4 {
            self.finish_local(&companion, &core, local, &local_prompt, &request_id)
                .await;
            return;
        }
        let query = text.chars().take(2000).collect::<String>();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.mind_recall(companion.clone(), query, 3, false, None, None),
        )
        .await;
        if self.active_request(&companion).as_deref() != Some(request_id.as_str()) {
            cancel(self);
            return;
        }
        let (memories, lexical, semantic, reason) = match result {
            Ok(Ok(result)) => (
                result.memories,
                result.lexical,
                result.semantic,
                result.reason,
            ),
            Ok(Err(_)) => (
                local.memories,
                local.lexical,
                "error".into(),
                Some("semantic_search_failed".into()),
            ),
            Err(_) => (
                local.memories,
                local.lexical,
                "timeout".into(),
                Some("semantic_timeout".into()),
            ),
        };
        let fragment = compose_recall_fragment(&companion, &memories, &lexical, &semantic);
        if self.active_request(&companion).as_deref() == Some(request_id.as_str()) {
            self.set_active_recall(
                &companion,
                ActiveRecall {
                    request_id: request_id.clone(),
                    memories,
                    lexical,
                    reason,
                },
            );
            finish(fragment);
        } else {
            cancel(self);
        }
    }

    async fn finish_local(
        &self,
        companion: &str,
        core: &str,
        local: RecallResult,
        local_prompt: &str,
        request_id: &str,
    ) {
        let finish = |prompt: String| {
            if let Ok(runtime) = self.rig() {
                let _ = runtime.complete_recall(request_id.to_owned(), prompt);
            }
        };
        let _ = (core, local_prompt);
        finish(compose_recall_fragment(
            companion,
            &local.memories,
            &local.lexical,
            "unavailable",
        ));
    }
}

/// Publish the committed state and refresh the live model input. Both happen
/// only after the SQLite commit; a failure here does not roll the mind back.
pub(super) async fn publish_mind(
    app: &HostApplication,
    companion: &str,
    core: &str,
    stored: StoredMind,
) -> Result<w::MindView> {
    let sequence = app.next_prompt_sequence();
    #[cfg(test)]
    if let Some(gate) = app.publish_gate().await {
        gate.block_first().await;
    }
    let index = app
        .0
        .repository
        .mind_index_status(companion.to_owned(), now())
        .await?;
    let consolidation = app
        .0
        .repository
        .mind_consolidation(
            companion.to_owned(),
            crate::application::memory::METHOD_REVISION.into(),
            now(),
        )
        .await?;
    let view = view(companion, core, stored.clone(), index, consolidation);
    if let Ok(runtime) = app.rig() {
        let active = app.active_recall(companion);
        let lexical = active
            .as_ref()
            .map(|recall| match &recall.reason {
                Some(reason) => format!("{}({reason})", recall.lexical),
                None => recall.lexical.clone(),
            })
            .unwrap_or_else(|| "idle".into());
        let prompt = compose_prompt_with(
            companion,
            core,
            &stored,
            active
                .as_ref()
                .map(|recall| recall.memories.as_slice())
                .unwrap_or_default(),
            &lexical,
            "lexical_only",
        );
        if let Err(error) = runtime.set_mind_prompt(companion.to_owned(), prompt, sequence) {
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
        let scope = call.scope;
        if !scope.allowed {
            let _ = call.reply.send(Err(tool_error(Error {
                kind: ErrorKind::PermissionRequired,
                reason: Some("mind_not_permitted"),
            })));
            return;
        }
        if scope.turn_start.is_empty() || scope.call_id.is_empty() {
            let _ = call.reply.send(Err(tool_error(Error {
                kind: ErrorKind::InvalidRequest,
                reason: Some("mind_context_incomplete"),
            })));
            return;
        }
        let companion = scope.companion.clone();
        let result = match call.kind {
            MindCallKind::Recall => match serde_json::from_value::<RecallArgs>(call.args) {
                Ok(args) => match self
                    .mind_recall(
                        companion,
                        args.query,
                        args.limit.unwrap_or(8).clamp(1, 20) as usize,
                        args.include_forgotten.unwrap_or(false),
                        args.since,
                        args.until,
                    )
                    .await
                {
                    Ok(result) => Ok(json!({
                        "count": result.memories.len(),
                        "memories": result.memories,
                        "search": {
                            "lexical": result.lexical,
                            "semantic": result.semantic,
                            "pending": result.pending,
                            "reason": result.reason,
                        }
                    })
                    .to_string()),
                    Err(error) => Err(tool_error(error)),
                },
                Err(_) => Err(tool_error(Error::invalid())),
            },
            MindCallKind::Read => match serde_json::from_value::<ReadArgs>(call.args) {
                Ok(args) => match self.mind_read(companion, args.id).await {
                    Ok(Some(memory)) => Ok(json!({"found": true, "memory": memory}).to_string()),
                    Ok(None) => Ok(json!({"found": false}).to_string()),
                    Err(error) => Err(tool_error(error)),
                },
                Err(_) => Err(tool_error(Error::invalid())),
            },
            MindCallKind::Update => match serde_json::from_value(call.args) {
                Ok(request) => match self
                    .apply_mind_update(
                        companion,
                        scope.operation_id().to_owned(),
                        request,
                        scope.sources.clone(),
                    )
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
