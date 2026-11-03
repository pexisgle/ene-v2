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
const SCAN_PAGES: usize = 8;
const RETRY_ATTEMPTS: u32 = 2;
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
        // Startup work: finish pending index and consolidation after a restart.
        self.index_once().await;
        self.consolidate_companions().await;
        loop {
            let mut consolidate = false;
            let mut shutdown = self.shutdown();
            tokio::select! {
                _ = self.0.mind_notify.notified() => consolidate = true,
                _ = shutdown.changed() => return,
                // The periodic tick only retries index work. A failed
                // consolidation waits for a new confirmed input, a config
                // change or a host restart instead of retrying every 5s.
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
            self.index_once().await;
            if consolidate {
                self.consolidate_companions().await;
                // A consolidation commit wakes the worker for indexing; do
                // not spin on that self-notification when no input changed.
                if *self.shutdown().borrow() {
                    return;
                }
            }
        }
    }

    async fn index_once(&self) {
        if let Ok(ids) = self.0.repository.mind_pending_fts(64, now()).await
            && !ids.is_empty()
            && let Err(error) = self.0.repository.mind_index_fts(ids.clone(), now()).await
        {
            let _ = self
                .0
                .repository
                .mind_mark_index_error(ids, "fts".into(), format!("{error:?}"), false, now())
                .await;
        }
        let Ok(pending) = self
            .0
            .repository
            .mind_pending_vectors(VECTOR_BATCH, now())
            .await
        else {
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
            match self.embed_memory(&owner, contents).await {
                Ok(Embedding { vectors, .. }) => {
                    let entries = ids
                        .iter()
                        .zip(vectors)
                        .map(|(memory_id, vector)| VectorEntry {
                            memory_id: memory_id.clone(),
                            model: super::inference::EMBEDDING_MODEL.into(),
                            preprocess: crate::infrastructure::sqlite::memory::TEXT_PREPROCESS
                                .into(),
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
                                false,
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
                            matches!(
                                error.kind,
                                ErrorKind::PermissionRequired
                                    | ErrorKind::UnsupportedCapability
                                    | ErrorKind::StateConflict
                            ),
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
            for _ in 0..8 {
                match self.consolidate_once(&companion.id).await {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }
    }

    async fn consolidate_once(&self, companion: &str) -> Result<bool, Error> {
        if *self.shutdown().borrow() {
            return Ok(false);
        }
        let status = self
            .0
            .repository
            .mind_consolidation(companion.to_owned(), METHOD_REVISION.into(), now())
            .await?;
        if *self.shutdown().borrow() {
            return Ok(false);
        }
        let mut pending = self
            .pending_messages(companion, status.last_sequence, status.last_offset)
            .await?;
        if pending.waiting {
            // Recheck the state, not a timer-as-confirmation: the assistant
            // entry may become visible one kernel frame after the turn event.
            for _ in 0..2 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                pending = self
                    .pending_messages(companion, status.last_sequence, status.last_offset)
                    .await?;
                if !pending.waiting {
                    break;
                }
            }
        }
        let (batch, sources, through, through_offset, chars) =
            batch_chunks(pending.messages, status.last_sequence, status.last_offset);
        if batch.is_empty() {
            if let Some((sequence, offset)) = pending.terminal_through
                && (sequence, offset) > (status.last_sequence, status.last_offset)
            {
                self.0
                    .repository
                    .mind_skip_consolidation(
                        companion.to_owned(),
                        sequence,
                        offset,
                        METHOD_REVISION.into(),
                        now(),
                    )
                    .await?;
                return Ok(true);
            }
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
        if !hooked && current.model_assignments.companion.as_option().is_none() {
            self.0
                .repository
                .mind_set_consolidation(
                    companion.to_owned(),
                    METHOD_REVISION.into(),
                    "unconfigured".into(),
                    Some("companion_model_unset".into()),
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
        let related = self.related_memories(companion, &batch).await;
        let instructions = consolidation_instructions();
        let payload = consolidation_payload(
            &stored,
            &current.mind,
            &related.relevant,
            &related.forgotten,
            &batch,
        );
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
        let mut attempt = 0u32;
        loop {
            let output = self
                .consolidation_model(companion, &instructions, &payload, &prompt)
                .await;
            let (text, finish) = match output {
                Ok(output) => output,
                Err(error) => {
                    if retryable(&error) && attempt + 1 < RETRY_ATTEMPTS {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
                    self.fail_consolidation(companion, &error).await;
                    return Err(error);
                }
            };
            if finish != "stop" {
                let error = Error::new(ErrorKind::ExternalFailure);
                if attempt + 1 < RETRY_ATTEMPTS {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                    continue;
                }
                self.fail_consolidation(companion, &error).await;
                return Err(error);
            }
            let parsed = match parse_consolidation(&text) {
                Ok(parsed) => parsed,
                Err(()) => {
                    let error = Error::new(ErrorKind::InvalidRequest);
                    if attempt + 1 < RETRY_ATTEMPTS {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
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
                    return Err(error);
                }
            };
            let _reason = parsed.reason;
            let request = MindUpdateRequest {
                expected_revision: stored.memory_revision,
                memories: parsed.memories,
                state: None,
            };
            if let Err(error) = request.validate() {
                self.fail_consolidation(companion, &error).await;
                return Err(error);
            }
            let operation_id = consolidation_operation_id(
                companion,
                status.last_sequence,
                status.last_offset,
                through,
                through_offset,
            );
            let request_hash = serde_json::to_string(&(
                companion,
                &request,
                &sources,
                through,
                through_offset,
                METHOD_REVISION,
            ))
            .map_err(|_| Error::invalid())?;
            match self
                .0
                .repository
                .mind_consolidate(
                    companion.to_owned(),
                    operation_id,
                    request_hash,
                    request,
                    sources.clone(),
                    through,
                    through_offset,
                    METHOD_REVISION.into(),
                    1,
                    chars,
                    now(),
                )
                .await
            {
                Ok(outcome) => {
                    if outcome.result.changed {
                        self.notify_mind_work();
                        let stored = self
                            .0
                            .repository
                            .mind_view(companion.to_owned(), now())
                            .await?;
                        publish_mind(self, companion, &current.mind, stored).await?;
                    }
                    // An already-applied range returns the past result but
                    // must not be announced as progress or replayed.
                    let _cursor = outcome.status.last_sequence;
                    return Ok(outcome.advanced);
                }
                Err(error) => {
                    if retryable(&error) && attempt + 1 < RETRY_ATTEMPTS {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                        continue;
                    }
                    self.fail_consolidation(companion, &error).await;
                    return Err(error);
                }
            }
        }
    }

    async fn fail_consolidation(&self, companion: &str, error: &Error) {
        let reason = error
            .reason
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{:?}", error.kind));
        let state = if error.kind == ErrorKind::UnsupportedCapability {
            "unconfigured"
        } else {
            "error"
        };
        let _ = self
            .0
            .repository
            .mind_set_consolidation(
                companion.to_owned(),
                METHOD_REVISION.into(),
                state.into(),
                Some(reason),
                now(),
            )
            .await;
    }

    async fn related_memories(&self, companion: &str, batch: &[Chunk]) -> RelatedMemories {
        let query = batch
            .iter()
            .map(|chunk| chunk.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let query = query.chars().take(2000).collect::<String>();
        let vector = self
            .embed_memory(companion, vec![query.clone()])
            .await
            .ok()
            .and_then(|embedding| embedding.vectors.into_iter().next());
        let relevant = self
            .0
            .repository
            .mind_search(
                companion.to_owned(),
                query.clone(),
                vector,
                Some(super::inference::EMBEDDING_MODEL.into()),
                8,
                false,
                None,
                None,
                now(),
            )
            .await
            .map(|result| result.memories)
            .unwrap_or_default();
        let forgotten = self
            .0
            .repository
            .mind_search(
                companion.to_owned(),
                query,
                None,
                None,
                8,
                true,
                None,
                None,
                now(),
            )
            .await
            .map(|result| {
                result
                    .memories
                    .into_iter()
                    .filter(|memory| memory.recall_preference == "forgotten")
                    .collect()
            })
            .unwrap_or_default();
        RelatedMemories {
            relevant,
            forgotten,
        }
    }

    async fn pending_messages(
        &self,
        companion: &str,
        last_sequence: u64,
        last_offset: u64,
    ) -> Result<PendingPage, Error> {
        let runtime = self.rig()?;
        let mut after = last_sequence.saturating_sub(1);
        let mut picked = Vec::new();
        let mut terminal_through = None;
        for _ in 0..SCAN_PAGES {
            let page = tokio::time::timeout(
                Duration::from_secs(2),
                runtime.history_after(companion.to_owned(), after, HISTORY_PAGE),
            )
            .await
            .map_err(|_| Error::unavailable())??;
            let (targets, last_terminal, waiting) =
                scan_page(&page.items, last_sequence, last_offset);
            picked.extend(targets);
            if waiting {
                break;
            }
            let dangling = unpaired_user(&picked);
            if !picked.is_empty() && !dangling {
                break;
            }
            if let Some(position) = last_terminal {
                terminal_through = Some(position);
            }
            let Some(last) = page.items.last().map(|message| message.sequence) else {
                break;
            };
            if page.next_cursor.is_none() || last <= after {
                break;
            }
            after = last;
        }
        // Keep the confirmed user even when its reply is not in the same
        // page. A short state recheck is allowed to pick up a reply that is
        // not visible yet; after that the user is processed rather than
        // waiting forever. Pages are never marked processed for a reply that
        // has not been confirmed.
        let waiting = unpaired_user(&picked);
        let has_picked = !picked.is_empty();
        Ok(PendingPage {
            messages: picked,
            terminal_through: if has_picked { None } else { terminal_through },
            waiting,
        })
    }

    async fn consolidation_model(
        &self,
        companion: &str,
        instructions: &str,
        payload: &str,
        prompt: &str,
    ) -> Result<(String, String), Error> {
        #[cfg(test)]
        if let Some(hook) = self.0.consolidation_hook.get() {
            return hook(prompt.to_owned())
                .await
                .map(|text| (text, "stop".into()));
        }
        #[cfg(not(test))]
        let _ = prompt;
        self.infer_memory(
            InferenceRequest {
                companion_id: companion.to_owned(),
                preamble: instructions.to_owned(),
                messages: vec![(true, payload.to_owned())],
                max_tokens: 4096,
            },
            CancellationToken::new(),
        )
        .await
        .map(|output| (output.text, output.finish))
    }
}

#[derive(Clone, Debug)]
struct Chunk {
    id: String,
    request_id: Option<String>,
    role: String,
    created_at: String,
    text: String,
    offset: usize,
    total: usize,
}

struct RelatedMemories {
    relevant: Vec<MemoryRecord>,
    forgotten: Vec<MemoryRecord>,
}

#[cfg(test)]
fn confirmed_message(message: &ene_rpc::Message) -> bool {
    // A user's completed entry is an accepted experience even when the turn
    // later failed. An assistant entry is spoken experience only when the
    // conversation records an answered turn; failed, stopped and open replies
    // stay out.
    message.status == "completed"
        && matches!(message.role.as_str(), "user" | "assistant")
        && !message.text.trim().is_empty()
}

struct PendingPage {
    messages: Vec<ene_rpc::Message>,
    terminal_through: Option<(u64, u64)>,
    waiting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryClass {
    Target,
    Terminal,
    Waiting,
}

fn classify_message(message: &ene_rpc::Message) -> EntryClass {
    match message.role.as_str() {
        "user"
            if message.status == "completed"
                && !message.text.trim().is_empty()
                && message.turn_status != "incomplete" =>
        {
            EntryClass::Target
        }
        "assistant" if message.status == "completed" && !message.text.trim().is_empty() => {
            EntryClass::Target
        }
        "assistant" if matches!(message.status.as_str(), "failed" | "cancelled") => {
            EntryClass::Terminal
        }
        "user" | "assistant" => EntryClass::Waiting,
        _ => EntryClass::Terminal,
    }
}

fn unpaired_user(messages: &[ene_rpc::Message]) -> bool {
    let Some(last) = messages.last() else {
        return false;
    };
    if last.role != "user" || last.turn_status != "completed" {
        return false;
    }
    let Some(request) = last.request_id.as_deref() else {
        return false;
    };
    !messages.iter().any(|message| {
        message.role == "assistant"
            && message.request_id.as_deref() == Some(request)
            && message.status == "completed"
    })
}

fn scan_page(
    messages: &[ene_rpc::Message],
    last_sequence: u64,
    last_offset: u64,
) -> (Vec<ene_rpc::Message>, Option<(u64, u64)>, bool) {
    let mut targets = Vec::new();
    let mut terminal = None;
    for message in messages {
        if message.sequence < last_sequence
            || (message.sequence == last_sequence
                && message.text.chars().count() <= last_offset as usize)
        {
            continue;
        }
        match classify_message(message) {
            EntryClass::Target => targets.push(message.clone()),
            EntryClass::Terminal => {
                terminal = Some((message.sequence, message.text.chars().count() as u64));
            }
            EntryClass::Waiting => return (targets, terminal, true),
        }
    }
    (targets, terminal, false)
}

fn retryable(error: &Error) -> bool {
    matches!(
        error.kind,
        ErrorKind::ProviderFailure | ErrorKind::ExternalFailure | ErrorKind::Unavailable
    )
}

fn batch_chunks(
    messages: Vec<ene_rpc::Message>,
    from_sequence: u64,
    from_offset: u64,
) -> (Vec<Chunk>, Vec<SourceRef>, u64, u64, u64) {
    let mut chunks = Vec::new();
    let mut chars = 0usize;
    let mut through_sequence = from_sequence;
    let mut through_offset = from_offset;
    for message in messages {
        let total = message.text.chars().count();
        let start = if message.sequence == from_sequence {
            from_offset as usize
        } else {
            0
        };
        if start >= total {
            continue;
        }
        let remaining = total - start;
        let room = BATCH_CHARS.saturating_sub(chars);
        if remaining > room && !chunks.is_empty() {
            break;
        }
        let take = remaining.min(room.max(1));
        let text = message
            .text
            .chars()
            .skip(start)
            .take(take)
            .collect::<String>();
        chunks.push(Chunk {
            id: message.id.clone(),
            request_id: message.request_id.clone(),
            role: message.role.clone(),
            created_at: message.created_at.clone(),
            text,
            offset: start,
            total,
        });
        chars += take;
        through_sequence = message.sequence;
        through_offset = (start + take) as u64;
        if take < remaining {
            break;
        }
        if chunks.len() >= BATCH_MESSAGES {
            break;
        }
    }
    let mut sources = Vec::new();
    for chunk in &chunks {
        let observed = Some(chunk.created_at.clone());
        match chunk.role.as_str() {
            "user" => sources.push(SourceRef {
                kind: SourceKind::Input,
                id: chunk.request_id.clone().unwrap_or_else(|| chunk.id.clone()),
                entry_id: Some(chunk.id.clone()),
                speaker: Some("user".into()),
                observed_at: observed,
            }),
            "assistant" => sources.push(SourceRef {
                kind: SourceKind::Reply,
                id: chunk.id.clone(),
                entry_id: Some(chunk.id.clone()),
                speaker: Some("assistant".into()),
                observed_at: observed,
            }),
            _ => {}
        }
    }
    (
        chunks,
        sources,
        through_sequence,
        through_offset,
        chars as u64,
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsolidationOutput {
    memories: Vec<NewMemory>,
    reason: String,
}

fn parse_consolidation(text: &str) -> std::result::Result<ConsolidationOutput, ()> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).map_err(|_| ())?;
    let object = value.as_object().ok_or(())?;
    // An empty or partial object is not an explicit no-change decision.
    if !object.contains_key("memories") || !object.contains_key("reason") {
        return Err(());
    }
    if object
        .keys()
        .any(|key| key != "memories" && key != "reason")
    {
        return Err(());
    }
    if object
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(());
    }
    serde_json::from_value(value).map_err(|_| ())
}

fn consolidation_operation_id(
    companion: &str,
    from: u64,
    from_offset: u64,
    through: u64,
    through_offset: u64,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"ene.mind.consolidation.v1\0");
    hash.update(companion.as_bytes());
    hash.update(from.to_le_bytes());
    hash.update(from_offset.to_le_bytes());
    hash.update(through.to_le_bytes());
    hash.update(through_offset.to_le_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn consolidation_instructions() -> String {
    "あなたはコンパニオンの経験整理担当です。以下はホストが確認済みの会話の出来事と既存の記憶です。\n\
     作者の会話例、未確定の生成文、内部推論、ツール内部の記録は経験として扱わないでください。\n\
     現在の感情・関係・作者設定・許可を直接変更してはいけません。出力は記憶の追加・訂正の提案だけです。\n\
     共に経験したこと、合意、成功・失敗の経緯、学んだ手順、好みを含められます。単なるプロフィール項目の羅列にしないでください。\n     ユーザーが共有した具体的な出来事（特に「一緒に何かをした」「うまくいった／失敗した」「こう決めた」）は、些細な雑談でない限り記憶として残してください。\n     確認済みの出来事にそのような経験が含まれるなら memories を空にしないでください。no_change は本当に残す経験・訂正・学びがない場合だけです。\n\
     既存の記憶と重複する内容は追加しないでください。訂正は新しい記憶の supersedes に既存 id を入れてください。\n\
     出力は次のJSONだけです（前後に文章を付けない）:\n\
     {\"memories\":[{\"content\":\"...\",\"description\":\"...\",\"appraisal\":\"...\",\"acquisition\":\"user_statement|direct_experience|report|shared|inference\",\"supersedes\":[\"既存id\"],\"sources\":[{\"kind\":\"memory\",\"id\":\"既存id\"}],\"occurred_at\":null}],\"reason\":\"短い理由\"}\n\
     変更がなければ {\"memories\":[],\"reason\":\"no_change\"} を返してください。"
        .into()
}

fn consolidation_payload(
    stored: &StoredMind,
    core: &str,
    relevant: &[MemoryRecord],
    forgotten: &[MemoryRecord],
    batch: &[Chunk],
) -> String {
    let entries = batch
        .iter()
        .map(|chunk| {
            serde_json::json!({
                "id": chunk.id,
                "time": chunk.created_at,
                "role": chunk.role,
                "text": chunk.text,
                "part_offset": chunk.offset,
                "message_chars": chunk.total,
            })
        })
        .collect::<Vec<_>>();
    let known = |memories: &[MemoryRecord]| {
        memories
            .iter()
            .map(|memory| {
                serde_json::json!({
                    "id": memory.id,
                    "revision": memory.revision,
                    "content": memory.content,
                    "description": memory.description,
                    "supersedes": memory.supersedes,
                    "recall_preference": memory.recall_preference,
                })
            })
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "author_material": {
            "core": core,
            "note": "作者設定と例はコンパニオンの語り方の材料。実体験ではない。",
            "profile": stored.profile,
        },
        "current_mind_revision": stored.state_revision,
        "consolidation_revision": stored.memory_revision,
        "current_mind": stored.state,
        "related_memories": known(relevant),
        "forgotten_memories": known(forgotten),
        "confirmed_entries": entries,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(sequence: u64, role: &str, status: &str, text: &str) -> ene_rpc::Message {
        ene_rpc::Message {
            id: format!("m{sequence}"),
            role: role.into(),
            text: text.into(),
            status: status.into(),
            sequence,
            created_at: "2026-01-01T00:00:00Z".into(),
            ..Default::default()
        }
    }

    #[test]
    fn long_entries_are_split_without_marking_the_tail_processed() {
        let long = "あ".repeat(10_000);
        let (chunks, sources, through, offset, chars) =
            batch_chunks(vec![message(1, "user", "completed", &long)], 0, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.chars().count(), 8_000);
        assert_eq!(through, 1);
        assert_eq!(offset, 8_000);
        assert_eq!(chars, 8_000);
        assert_eq!(sources.len(), 1);
        let (chunks, _, through, offset, chars) =
            batch_chunks(vec![message(1, "user", "completed", &long)], 1, 8_000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.chars().count(), 2_000);
        assert_eq!(through, 1);
        assert_eq!(offset, 10_000);
        assert_eq!(chars, 2_000);
        assert!(chunks[0].text.chars().all(|ch| ch == 'あ'));
    }

    #[test]
    fn only_completed_user_and_answered_assistant_entries_are_confirmed() {
        assert!(confirmed_message(&message(1, "user", "completed", "経験")));
        assert!(!confirmed_message(&message(
            2,
            "assistant",
            "failed",
            "失敗"
        )));
        assert!(!confirmed_message(&message(
            3,
            "assistant",
            "cancelled",
            "停止"
        )));
        assert!(!confirmed_message(&message(
            4,
            "assistant",
            "incomplete",
            "未完了"
        )));
        assert!(confirmed_message(&message(
            5,
            "assistant",
            "completed",
            "答えた"
        )));
        assert!(!confirmed_message(&message(6, "user", "completed", "  ")));
    }

    #[tokio::test]
    async fn ten_thousand_character_tail_survives_store_reopen_and_replay() {
        use crate::domain::mind::{
            Acquisition, MindUpdateRequest, NewMemory, SourceKind, SourceRef,
        };
        use crate::infrastructure::sqlite::Store;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.db");
        let store = Store::open(path.clone()).await.unwrap();
        store
            .mind_update(
                "c".into(),
                "seed".into(),
                "seed-hash".into(),
                MindUpdateRequest {
                    expected_revision: 1,
                    memories: vec![NewMemory {
                        content: "既存".into(),
                        description: String::new(),
                        appraisal: String::new(),
                        acquisition: Acquisition::DirectExperience,
                        supersedes: Vec::new(),
                        sources: Vec::new(),
                        occurred_at: None,
                    }],
                    state: None,
                },
                vec![SourceRef {
                    kind: SourceKind::Input,
                    id: "seed".into(),
                    entry_id: Some("seed".into()),
                    speaker: Some("user".into()),
                    observed_at: Some("2026-01-01T00:00:00Z".into()),
                }],
                "parent".into(),
                "2026-01-01T00:00:00Z".into(),
            )
            .await
            .unwrap();
        let text = "終".repeat(10_000);
        let message = message(1, "user", "completed", &text);
        let (chunks, sources, through, offset, chars) = batch_chunks(vec![message.clone()], 0, 0);
        assert_eq!(chunks.len(), 1);
        assert_eq!((through, offset, chars), (1, 8_000, 8_000));
        let first = store
            .mind_consolidate(
                "c".into(),
                "long-1".into(),
                "h1".into(),
                MemoryUpdateEmpty::request(2),
                sources,
                through,
                offset,
                METHOD_REVISION.into(),
                1,
                chars,
                "2026-01-01T00:00:01Z".into(),
            )
            .await
            .unwrap();
        assert!(first.advanced);
        assert_eq!(first.status.last_offset, 8_000);
        // A new connection is the restart boundary; the original host's
        // SQLite connection remains open just as a second host would see it.
        let store = Store::open(path).await.unwrap();
        let (chunks, sources, through, offset, chars) = batch_chunks(vec![message], 1, 8_000);
        assert_eq!(chunks.len(), 1);
        assert_eq!((through, offset, chars), (1, 10_000, 2_000));
        let last = store
            .mind_consolidate(
                "c".into(),
                "long-2".into(),
                "h2".into(),
                MemoryUpdateEmpty::request(2),
                sources,
                through,
                offset,
                METHOD_REVISION.into(),
                1,
                chars,
                "2026-01-01T00:00:02Z".into(),
            )
            .await
            .unwrap();
        assert!(last.advanced);
        assert_eq!(last.status.last_offset, 10_000);
        let replay = store
            .mind_consolidate(
                "c".into(),
                "long-2".into(),
                "h2".into(),
                MemoryUpdateEmpty::request(2),
                vec![],
                through,
                offset,
                METHOD_REVISION.into(),
                1,
                chars,
                "2026-01-01T00:00:03Z".into(),
            )
            .await
            .unwrap();
        assert!(!replay.advanced);
        assert_eq!(replay.status.last_offset, 10_000);
    }

    struct MemoryUpdateEmpty;
    impl MemoryUpdateEmpty {
        fn request(revision: u64) -> crate::domain::mind::MindUpdateRequest {
            crate::domain::mind::MindUpdateRequest {
                expected_revision: revision,
                memories: Vec::new(),
                state: None,
            }
        }
    }

    #[test]
    fn terminal_failed_pages_are_skipped_but_open_replies_wait() {
        assert_eq!(
            classify_message(&message(1, "assistant", "failed", "失敗")),
            EntryClass::Terminal
        );
        assert_eq!(
            classify_message(&message(2, "assistant", "cancelled", "停止")),
            EntryClass::Terminal
        );
        assert_eq!(
            classify_message(&message(3, "assistant", "incomplete", "未完了")),
            EntryClass::Waiting
        );
        assert_eq!(
            classify_message(&message(4, "user", "completed", "確定した発言")),
            EntryClass::Target
        );
        assert_eq!(
            classify_message(&message(5, "assistant", "completed", "答えた")),
            EntryClass::Target
        );
    }

    #[test]
    fn failed_pages_are_passed_until_a_later_confirmed_user_input() {
        let failed = (1..=3)
            .map(|sequence| message(sequence, "assistant", "failed", "失敗"))
            .collect::<Vec<_>>();
        let user = vec![message(4, "user", "completed", "確定した発言")];
        let (targets, terminal, waiting) = scan_page(&failed, 0, 0);
        assert!(targets.is_empty());
        assert_eq!(terminal, Some((3, 2)));
        assert!(!waiting);
        let (targets, _, waiting) = scan_page(&user, 0, 0);
        assert_eq!(targets.len(), 1);
        assert!(!waiting);
    }

    #[test]
    fn an_open_reply_stops_the_scan_instead_of_being_skipped() {
        let page = vec![
            message(1, "assistant", "failed", "失敗"),
            message(2, "assistant", "incomplete", "未完了"),
            message(3, "user", "completed", "後続"),
        ];
        let (targets, terminal, waiting) = scan_page(&page, 0, 0);
        assert!(targets.is_empty());
        assert_eq!(terminal, Some((1, 2)));
        assert!(waiting);
    }

    #[test]
    fn invalid_consolidation_output_is_distinct_from_no_change() {
        assert!(parse_consolidation("not json").is_err());
        assert!(parse_consolidation(r#"{"memories":[] ,"reason":"no_change"}"#).is_ok());
        assert!(parse_consolidation(
            r#"{"memories":[{"content":"経験","acquisition":"direct_experience"}],"reason":"x"}"#
        )
        .is_ok());
        // An unknown top-level field is not silently applied as a state change.
        assert!(parse_consolidation(r#"{"memories":[],"reason":"x","state":{}}"#).is_err());
        assert!(parse_consolidation(r#"{}"#).is_err());
        assert!(parse_consolidation(r#"{"memories":[]}"#).is_err());
        assert!(parse_consolidation(r#"{"memories":[],"reason":""}"#).is_err());
    }

    #[test]
    fn consolidation_ids_cover_the_range_and_method() {
        let one = consolidation_operation_id("a", 1, 0, 5, 0);
        assert_eq!(one, consolidation_operation_id("a", 1, 0, 5, 0));
        assert_ne!(one, consolidation_operation_id("a", 1, 0, 6, 0));
        assert_ne!(one, consolidation_operation_id("a", 1, 2, 5, 0));
        assert_ne!(one, consolidation_operation_id("a", 1, 0, 5, 10));
    }
}
