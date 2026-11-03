//! Background index and consolidation work. Finite host tasks use the task
//! grant; they are not a second personality or a general job framework.

use super::{
    HostApplication,
    mind::{now, publish_mind},
};
use crate::{
    domain::{
        Error, ErrorKind,
        mind::{MemoryRecord, MindUpdateRequest, NewMemory, SourceKind, SourceRef, StoredMind},
    },
    infrastructure::sqlite::memory::VectorEntry,
    llm::Embedding,
    ports::inference::InferenceRequest,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(crate) const METHOD_REVISION: &str = "consolidation-v1";
const BATCH_MESSAGES: usize = 12;
const BATCH_CHARS: usize = 8_000;
const HISTORY_PAGE: usize = 64;
const HISTORY_PAGES: usize = 4;
const VECTOR_BATCH: usize = 16;

impl HostApplication {
    /// Wake the finite index/consolidation worker. Safe to call repeatedly.
    pub(crate) fn start_mind_worker(&self) {
        if self
            .0
            .mind_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let app = self.clone();
        self.0
            .background
            .spawn(async move { app.run_mind_worker().await });
    }

    pub(crate) fn notify_mind_work(&self) {
        self.0.mind_notify.notify_one();
    }

    async fn run_mind_worker(&self) {
        loop {
            if *self.shutdown().borrow() {
                return;
            }
            self.index_once().await;
            self.consolidate_companions().await;
            let mut shutdown = self.shutdown();
            tokio::select! {
                _ = self.0.mind_notify.notified() => {}
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }

    async fn index_once(&self) {
        if let Ok(ids) = self.0.repository.mind_pending_fts(64).await
            && !ids.is_empty()
            && let Err(error) = self.0.repository.mind_index_fts(ids.clone(), now()).await
        {
            let _ = self
                .0
                .repository
                .mind_mark_index_error(ids, "fts".into(), format!("{error:?}"), now())
                .await;
        }
        let Ok(pending) = self.0.repository.mind_pending_vectors(VECTOR_BATCH).await else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        let mut by_owner: std::collections::BTreeMap<String, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for (id, owner, content) in pending {
            by_owner.entry(owner).or_default().push((id, content));
        }
        for (owner, batch) in by_owner {
            let ids = batch.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
            let contents = batch
                .into_iter()
                .map(|(_, content)| content)
                .collect::<Vec<_>>();
            match self.embed_task(&owner, contents).await {
                Ok(Embedding { vectors, .. }) => {
                    let entries = ids
                        .iter()
                        .zip(vectors)
                        .map(|(memory_id, vector)| VectorEntry {
                            memory_id: memory_id.clone(),
                            model: super::inference::EMBEDDING_MODEL.into(),
                            vector,
                        })
                        .collect::<Vec<_>>();
                    if let Err(error) = self
                        .0
                        .repository
                        .mind_store_vectors(
                            entries,
                            super::inference::EMBEDDING_MODEL.into(),
                            now(),
                        )
                        .await
                    {
                        let _ = self
                            .0
                            .repository
                            .mind_mark_index_error(
                                ids,
                                "vector".into(),
                                format!("{error:?}"),
                                now(),
                            )
                            .await;
                    }
                }
                Err(error) => {
                    let _ = self
                        .0
                        .repository
                        .mind_mark_index_error(
                            ids,
                            "vector".into(),
                            error
                                .reason
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{:?}", error.kind)),
                            now(),
                        )
                        .await;
                    let _ = self
                        .0
                        .repository
                        .mind_set_index_state(
                            owner,
                            if error.kind == ErrorKind::UnsupportedCapability
                                || error.kind == ErrorKind::StateConflict
                            {
                                "unavailable".into()
                            } else {
                                "error".into()
                            },
                            error.reason.map(str::to_owned),
                            None,
                            now(),
                        )
                        .await;
                }
            }
        }
    }

    async fn consolidate_companions(&self) {
        let Ok(companions) = self.companions().await else {
            return;
        };
        for companion in companions {
            for _ in 0..2 {
                match self.consolidate_once(&companion.id).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }
    }

    async fn consolidate_once(&self, companion: &str) -> Result<bool, Error> {
        // Let the frame that answered the turn write its confirmed reply and
        // the session log before the batch is read. The wait is bounded and
        // batches rapid turns instead of discarding a running job.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = self
            .0
            .repository
            .mind_consolidation(companion.to_owned(), METHOD_REVISION.into(), now())
            .await?;
        let messages = self
            .pending_messages(companion, status.last_sequence)
            .await?;
        if messages.is_empty() {
            if status.state != "idle" {
                let _ = self
                    .0
                    .repository
                    .mind_set_consolidation(
                        companion.to_owned(),
                        METHOD_REVISION.into(),
                        "idle".into(),
                        None,
                        now(),
                    )
                    .await;
            }
            return Ok(false);
        }
        let current = self.companion(companion).await?;
        #[cfg(test)]
        let hooked = self.0.consolidation_hook.get().is_some();
        #[cfg(not(test))]
        let hooked = false;
        if !hooked && current.model_assignments.task.as_option().is_none() {
            self.0
                .repository
                .mind_set_consolidation(
                    companion.to_owned(),
                    METHOD_REVISION.into(),
                    "unconfigured".into(),
                    Some("task_model_unset".into()),
                    now(),
                )
                .await?;
            return Ok(false);
        }
        let stored = self
            .0
            .repository
            .mind_view(companion.to_owned(), now())
            .await?;
        let memories = self
            .0
            .repository
            .mind_recent(companion.to_owned(), 8)
            .await?;
        let (batch, sources, through, chars) = self.batch(messages, status.last_sequence);
        if batch.is_empty() {
            return Ok(false);
        }
        let instructions = consolidation_instructions();
        let payload = consolidation_payload(&stored, &memories, &batch);
        let prompt = format!("{instructions}\n\n{payload}");
        self.0
            .repository
            .mind_set_consolidation(
                companion.to_owned(),
                METHOD_REVISION.into(),
                "running".into(),
                None,
                now(),
            )
            .await?;
        let output = match self
            .consolidation_model(companion, &instructions, &payload, &prompt)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                let _ = self
                    .0
                    .repository
                    .mind_set_consolidation(
                        companion.to_owned(),
                        METHOD_REVISION.into(),
                        "error".into(),
                        Some(
                            error
                                .reason
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{:?}", error.kind)),
                        ),
                        now(),
                    )
                    .await;
                return Err(error);
            }
        };
        let parsed: ConsolidationOutput = match parse_consolidation(&output) {
            Ok(parsed) => parsed,
            Err(()) => {
                let _ = self
                    .0
                    .repository
                    .mind_set_consolidation(
                        companion.to_owned(),
                        METHOD_REVISION.into(),
                        "error".into(),
                        Some("generation_invalid".into()),
                        now(),
                    )
                    .await;
                return Ok(false);
            }
        };
        let _reason = parsed.reason.clone();
        let request = MindUpdateRequest {
            expected_revision: stored.state_revision,
            memories: parsed.memories,
            state: None,
        };
        request.validate()?;
        let operation_id = consolidation_operation_id(companion, status.last_sequence, through);
        let request_hash =
            serde_json::to_string(&(companion, &request, &sources, through, METHOD_REVISION))
                .map_err(|_| Error::invalid())?;
        match self
            .0
            .repository
            .mind_consolidate(
                companion.to_owned(),
                operation_id,
                request_hash,
                request,
                sources,
                through,
                METHOD_REVISION.into(),
                1,
                chars,
                now(),
            )
            .await
        {
            Ok((result, _)) => {
                self.notify_mind_work();
                if result.changed {
                    let stored = self
                        .0
                        .repository
                        .mind_view(companion.to_owned(), now())
                        .await?;
                    publish_mind(self, companion, &current.mind, stored).await?;
                }
                Ok(true)
            }
            Err(error) => {
                let _ = self
                    .0
                    .repository
                    .mind_set_consolidation(
                        companion.to_owned(),
                        METHOD_REVISION.into(),
                        "error".into(),
                        Some(
                            error
                                .reason
                                .map(str::to_owned)
                                .unwrap_or_else(|| format!("{:?}", error.kind)),
                        ),
                        now(),
                    )
                    .await;
                Err(error)
            }
        }
    }

    async fn pending_messages(
        &self,
        companion: &str,
        last_sequence: u64,
    ) -> Result<Vec<ene_rpc::Message>, Error> {
        let runtime = self.rig()?;
        let mut cursor = None;
        let mut picked = Vec::new();
        for _ in 0..HISTORY_PAGES {
            let page = runtime
                .history(companion.to_owned(), cursor.clone(), HISTORY_PAGE)
                .await?;
            let oldest = page
                .items
                .first()
                .map(|message| message.sequence)
                .unwrap_or(0);
            for message in page.items {
                if message.sequence > last_sequence
                    && matches!(message.role.as_str(), "user" | "assistant")
                    && !message.text.trim().is_empty()
                {
                    picked.push(message);
                }
            }
            if oldest <= last_sequence || page.next_cursor.is_none() {
                break;
            }
            cursor = page.next_cursor;
        }
        picked.sort_by_key(|message| message.sequence);
        picked.dedup_by_key(|message| message.id.clone());
        Ok(picked)
    }

    fn batch(
        &self,
        messages: Vec<ene_rpc::Message>,
        from: u64,
    ) -> (Vec<ene_rpc::Message>, Vec<SourceRef>, u64, u64) {
        let mut batch = Vec::new();
        let mut chars = 0;
        for message in messages {
            let len = message.text.chars().count();
            if !batch.is_empty() && (batch.len() >= BATCH_MESSAGES || chars + len > BATCH_CHARS) {
                break;
            }
            chars += len;
            batch.push(message);
        }
        let through = batch.last().map(|message| message.sequence).unwrap_or(from);
        let mut sources = Vec::new();
        for message in &batch {
            let id = message.id.clone();
            let observed = Some(message.created_at.clone());
            match message.role.as_str() {
                "user" => sources.push(SourceRef {
                    kind: SourceKind::Input,
                    id: message.request_id.clone().unwrap_or_else(|| id.clone()),
                    entry_id: Some(id),
                    speaker: Some("user".into()),
                    observed_at: observed,
                }),
                "assistant" => sources.push(SourceRef {
                    kind: SourceKind::Reply,
                    id: id.clone(),
                    entry_id: Some(id),
                    speaker: Some("assistant".into()),
                    observed_at: observed,
                }),
                _ => {}
            }
        }
        (batch, sources, through, chars as u64)
    }

    async fn consolidation_model(
        &self,
        companion: &str,
        instructions: &str,
        payload: &str,
        prompt: &str,
    ) -> Result<String, Error> {
        #[cfg(test)]
        if let Some(hook) = self.0.consolidation_hook.get() {
            return hook(prompt.to_owned()).await;
        }
        #[cfg(not(test))]
        let _ = prompt;
        self.infer_task(
            InferenceRequest {
                companion_id: companion.to_owned(),
                preamble: instructions.to_owned(),
                messages: vec![(true, payload.to_owned())],
                max_tokens: 4096,
            },
            CancellationToken::new(),
        )
        .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsolidationOutput {
    #[serde(default)]
    memories: Vec<NewMemory>,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_consolidation(text: &str) -> std::result::Result<ConsolidationOutput, ()> {
    let trimmed = text.trim();
    let start = trimmed.find('{').ok_or(())?;
    let end = trimmed.rfind('}').ok_or(())?;
    if end < start {
        return Err(());
    }
    serde_json::from_str(&trimmed[start..=end]).map_err(|_| ())
}

fn consolidation_operation_id(companion: &str, from: u64, through: u64) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ene.mind.consolidation.v1\0");
    hash.update(companion.as_bytes());
    hash.update(from.to_le_bytes());
    hash.update(through.to_le_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn consolidation_instructions() -> String {
    "あなたはコンパニオンの経験整理担当です。以下はホストが確認済みの会話の出来事と既存の記憶です。\n\
     作者の会話例、未確定の生成文、内部推論、ツール内部の記録は経験として扱わないでください。\n\
     現在の感情・関係・作者設定・許可を直接変更してはいけません。出力は記憶の追加・訂正の提案だけです。\n\
     共に経験したこと、合意、成功・失敗の経緯、学んだ手順、好みを含められます。単なるプロフィール項目の羅列にしないでください。\n     ユーザーが共有した具体的な出来事（特に「一緒に何かをした」「うまくいった／失敗した」「こう決めた」）は、些細な雑談でない限り記憶として残してください。\n\
     既存の記憶と重複する内容は追加しないでください。訂正は新しい記憶の supersedes に既存 id を入れてください。\n\
     出力は次のJSONだけです（前後に文章を付けない）:\n\
     {\"memories\":[{\"content\":\"...\",\"description\":\"...\",\"appraisal\":\"...\",\"acquisition\":\"user_statement|direct_experience|report|shared|inference\",\"supersedes\":[\"既存id\"],\"sources\":[{\"kind\":\"memory\",\"id\":\"既存id\"}],\"occurred_at\":null}],\"reason\":\"短い理由\"}\n\
     変更がなければ {\"memories\":[],\"reason\":\"no_change\"} を返してください。"
        .into()
}

fn consolidation_payload(
    stored: &StoredMind,
    memories: &[MemoryRecord],
    batch: &[ene_rpc::Message],
) -> String {
    let entries = batch
        .iter()
        .map(|message| {
            serde_json::json!({
                "id": message.id,
                "time": message.created_at,
                "role": message.role,
                "text": message.text.chars().take(2000).collect::<String>(),
            })
        })
        .collect::<Vec<_>>();
    let known = memories
        .iter()
        .map(|memory| {
            serde_json::json!({
                "id": memory.id,
                "revision": memory.revision,
                "content": memory.content,
                "description": memory.description,
                "supersedes": memory.supersedes,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "current_mind_revision": stored.state_revision,
        "current_mind": stored.state,
        "existing_memories": known,
        "confirmed_entries": entries,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_consolidation_output_is_distinct_from_no_change() {
        assert!(parse_consolidation("not json").is_err());
        assert!(parse_consolidation(r#"{"memories":[] ,"reason":"no_change"}"#).is_ok());
        assert!(
            parse_consolidation(
                r#"{"memories":[{"content":"経験","acquisition":"direct_experience"}]}"#
            )
            .is_ok()
        );
        // An unknown top-level field is not silently applied as a state change.
        assert!(parse_consolidation(r#"{"memories":[],"state":{}}"#).is_err());
    }

    #[test]
    fn consolidation_ids_cover_the_range_and_method() {
        let one = consolidation_operation_id("a", 1, 5);
        assert_eq!(one, consolidation_operation_id("a", 1, 5));
        assert_ne!(one, consolidation_operation_id("a", 2, 5));
        assert_ne!(one, consolidation_operation_id("b", 1, 5));
        assert_ne!(one, consolidation_operation_id("a", 1, 6));
    }
}
