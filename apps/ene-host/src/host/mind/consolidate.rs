//! Compaction's memory pass: the turns one summary stands for are read once, a
//! side model extracts the facts and the mind that interval moved, and the host
//! validates citations and commits memories, edges and the reflection
//! (docs/design/compaction.md §3.2, docs/design/mind.md §5.2).
//!
//! The pass is one async function the compaction job awaits. Nothing schedules
//! it and nothing stores where it stands: its range is the message window
//! between the previous summary and the new one, so the newest `ene.summary`
//! aside is the only position (docs/design/compaction.md §2.1).
use super::{
    Companion, Role,
    embedding::{self, Embeddings},
};
use crate::host::{
    db::{
        Result, Store,
        memories::{MindStateChange, MindUpdate, StateUpdate},
    },
    error::HostError,
    model::{self, Credentials},
};
use rig_agent::extractor::ExtractorBuilder;
use rig_core::completion::Message;
use rig_core::conversation::{Conversation as CoreConversation, TurnEnd};
use rig_core::{DynModel, operation::Completion};
use rig_ecs::model::Models;

/// How many candidate memories the extraction model may merge into.
const MAX_CONTEXT: usize = 20;
/// The longest turn text used as the retrieval query for merge candidates.
const MAX_QUERY_CHARS: usize = 2_000;
/// How many newest turns are joined into that query.
const QUERY_TURNS: usize = 4;

use ene_mind::{Batch, Commit, EXTRACTION_SYSTEM_PROMPT, Extraction, Turn, extraction_prompt};

/// The turns inside the message-id window `(after, through]`, in conversation
/// order: exactly the turns the summary this pass runs beside stands for.
///
/// Message ids start at 0, so "no earlier summary" is `None`: a number cannot
/// also mean "nothing yet" without skipping the first turn.
pub(crate) fn pending_turns(
    conversation: &CoreConversation,
    after: Option<u64>,
    through: u64,
) -> Vec<Turn> {
    let mut records = conversation.turns().peekable();
    let mut turns = Vec::new();
    while let Some((_, record)) = records.next() {
        let start = record.start.get();
        if start > through || after.is_some_and(|after| start <= after) {
            continue;
        }
        if !matches!(record.end, Some(TurnEnd::Answered { .. })) {
            continue;
        }
        let from = conversation.start_of(record.start);
        let to = records
            .peek()
            .map_or(conversation.entries().len(), |(_, next)| {
                conversation.start_of(next.start)
            });
        let mut text = String::new();
        for entry in conversation
            .entries()
            .skip(from)
            .take(to.saturating_sub(from))
        {
            if entry.is_stop_marker() {
                continue;
            }
            let Some(line) = crate::host::agent::entry_text(&entry) else {
                continue;
            };
            let speaker = if entry.is_users_own() {
                "ユーザー"
            } else if matches!(entry.message, Message::Assistant(_)) {
                "コンパニオン"
            } else {
                continue;
            };
            text.push_str(speaker);
            text.push_str(": ");
            text.push_str(&line);
            text.push('\n');
        }
        if text.trim().is_empty() {
            continue;
        }
        turns.push(Turn {
            turn_id: start.to_string(),
            text,
        });
    }
    turns
}

/// The host services one pass reads. Cloned into the compaction job, so the
/// pass owns what it needs instead of borrowing the world.
#[derive(Clone)]
pub(crate) struct Services {
    pub(crate) store: Store,
    pub(crate) embeddings: Embeddings,
    pub(crate) credentials: Option<Credentials>,
    pub(crate) models: Models,
    pub(crate) providers: crate::host::providers::Providers,
}

/// Runs the one consolidation pass over the window `(after, through]` and
/// commits it. A pass that fails commits nothing.
pub(crate) async fn pass(
    services: &Services,
    companion_id: &str,
    conversation: &CoreConversation,
    after: Option<u64>,
    through: u64,
) -> Result<Commit> {
    let turns = pending_turns(conversation, after, through);
    if turns.is_empty() {
        return Ok(Commit::default());
    }
    let companion = services
        .store
        .companion_repo()
        .companion(companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let target = target(services, &companion)?;
    // Merge candidates come from the same hybrid retrieval recall uses: FTS5
    // over the newest turns plus vector similarity, with pinned and important
    // memories only as a fallback. Forgotten memories surface when the topic
    // matches, so the model sees that the user asked to forget them.
    let query = turn_query(&turns);
    let semantic = embedding::semantic_candidates(
        &services.store,
        &services.embeddings,
        services.credentials.as_ref(),
        companion_id,
        &query,
        ene_mind::RecallMode::Explicit,
    )
    .await;
    let context = services.store.memory_repo().consolidation_context(
        companion_id,
        &query,
        &semantic,
        MAX_CONTEXT,
    )?;
    let injected = services
        .store
        .memory_repo()
        .injected_between(companion_id, after, through)?;
    let extraction = extract(
        &target,
        extraction_prompt(&turns, &context, &injected, &companion.state),
    )
    .await?;
    let batch = Batch {
        companion_id: companion_id.to_owned(),
        through,
        context,
        injected,
    };
    let commit = services
        .store
        .memory_repo()
        .consolidate(&batch, &extraction.facts)?;
    reflect(services, &companion, through, extraction.state)?;
    Ok(commit)
}

/// Resolves the companion's `Consolidation` assignment (or its documented
/// companion fallback) to a connected model.
fn target(services: &Services, companion: &Companion) -> Result<DynModel<Completion>> {
    let credentials = services.credentials.as_ref().ok_or_else(|| {
        HostError::Unavailable(("no credentials are loaded for consolidation").into())
    })?;
    model::connect(
        &services.store,
        credentials,
        services.models.0.catalog(),
        companion,
        Role::Consolidation,
        &services.providers,
    )
}

/// Applies the emotion and growth the interval left through the same path
/// `mind_update` uses: one `mind_operations` receipt per window, and the mind
/// document it read as the revision the write has to match.
fn reflect(
    services: &Services,
    companion: &Companion,
    through: u64,
    patch: Option<StateUpdate>,
) -> Result<()> {
    let Some(patch) = patch else {
        return Ok(());
    };
    let mut next = companion.state.clone();
    patch.apply(&mut next);
    let state = serde_json::to_string(&next)
        .map_err(|error| HostError::Internal(format!("companion state: {error}")))?;
    let operation_id = format!("consolidate:{}:{through}", companion.id);
    let request = serde_json::json!({
        "operation_id": &operation_id,
        "memories": [],
        "state": &patch,
    });
    let request_hash = services.store.memory_repo().mind_hash(&request);
    match services.store.memory_repo().apply_mind_update(&MindUpdate {
        operation_id,
        companion_id: companion.id.clone(),
        request_hash,
        memories: Vec::new(),
        state: Some(MindStateChange {
            expected_revision: companion.state_revision,
            state,
        }),
    }) {
        Ok(_) => Ok(()),
        // A mind edit that landed while the pass ran wins: the reflection of
        // this window is discarded, and the rest of the pass still counts
        // (docs/design/mind.md §5.2).
        Err(ene_store::StoreError::Conflict(why)) => {
            tracing::debug!(
                companion = %companion.id,
                %why,
                "the mind moved while consolidation ran; its reflection was discarded"
            );
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// The provider must submit typed facts. Text and fenced JSON are not accepted.
async fn extract(model: &DynModel<Completion>, prompt: String) -> Result<Extraction> {
    let options = model
        .spec()
        .map_or_default(|spec| spec.default_options(None));
    let extraction = ExtractorBuilder::<Extraction>::new(model.clone())
        .append_preamble(EXTRACTION_SYSTEM_PROMPT)
        .retries(0)
        .build()
        .extract(prompt)
        .options(options)
        .await
        .map_err(|error| HostError::Unavailable(format!("consolidation output: {error}")))?;
    Ok(Extraction {
        facts: ene_mind::normalize(extraction.output.facts),
        state: extraction.output.state,
    })
}

/// The retrieval query for a pass's merge candidates: the newest turns' text,
/// bounded so a long transcript cannot become an unbounded FTS expression.
fn turn_query(turns: &[Turn]) -> String {
    let from = turns.len().saturating_sub(QUERY_TURNS);
    let mut query = String::new();
    for turn in &turns[from..] {
        query.push_str(&turn.text);
        query.push('\n');
    }
    ene_store::util::truncate(&query, MAX_QUERY_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_mind::{Fact, MemoryContext};
    use ene_mind::{MemoryRecord, validate_citations};
    use rig_core::conversation::MessageId;

    fn answer(conversation: &mut CoreConversation, user: &str, reply: &str) -> MessageId {
        let start = conversation.push(Message::user(user));
        conversation.open_turn();
        conversation.push(Message::assistant(reply));
        conversation.end_turn(TurnEnd::Answered { answer: None });
        start
    }

    /// The newest message id of `conversation`: the end of the window a pass
    /// over the whole log would cover.
    fn newest(conversation: &CoreConversation) -> u64 {
        conversation
            .entries()
            .next_back()
            .map_or(0, |entry| entry.id.get())
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn batch(through: u64, context: Vec<MemoryContext>) -> Batch {
        Batch {
            companion_id: "ene".into(),
            through,
            context,
            injected: Vec::new(),
        }
    }

    fn new_memory(id: &str, content: &str) -> MemoryRecord {
        MemoryRecord {
            id: id.into(),
            scope: "ene".into(),
            content: content.into(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    fn context(memory: &ene_mind::Memory) -> MemoryContext {
        MemoryContext {
            id: memory.id.clone(),
            revision: memory.memory_revision,
            importance: memory.importance,
            content: memory.content.clone(),
        }
    }

    fn fact() -> serde_json::Value {
        serde_json::json!([{
            "content": "紅茶の店は新宿にある",
            "importance": 7,
            "scope": "",
            "supersedes": "m1",
            "cited_memory_ids": []
        }])
    }

    /// The window rule: only answered turns whose start lies inside
    /// `(after, through]` are read, and the first turn is not lost.
    #[test]
    fn pending_turns_reads_only_answered_turns_inside_the_window() {
        let mut conversation = CoreConversation::new();
        let first = answer(&mut conversation, "hello", "hi");
        let second = answer(&mut conversation, "my name is A", "noted");
        let third = answer(&mut conversation, "and B", "noted");
        // A turn that never ended is not consolidated.
        conversation.push(Message::user("unfinished"));
        conversation.open_turn();
        conversation.push(Message::assistant("cut off"));

        let turns = pending_turns(&conversation, None, newest(&conversation));
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[0].turn_id, first.get().to_string());
        assert_eq!(turns[1].turn_id, second.get().to_string());
        assert_eq!(turns[2].turn_id, third.get().to_string());
        assert!(turns[1].text.contains("my name is A"));
        assert!(!turns[1].text.contains("and B"), "a turn holds its own log");

        let only_first = pending_turns(&conversation, None, first.get() + 1);
        assert_eq!(only_first.len(), 1);
        assert_eq!(
            only_first[0].turn_id,
            first.get().to_string(),
            "the first turn starts at message 0, so no summary yet reads it"
        );
        let after_first = pending_turns(&conversation, Some(first.get() + 1), third.get() + 1);
        assert_eq!(after_first.len(), 2, "the window is (after, through]");
        assert_eq!(after_first[0].turn_id, second.get().to_string());
        assert!(
            pending_turns(&conversation, Some(second.get() + 1), second.get() + 1).is_empty(),
            "an empty window reads nothing"
        );
    }

    #[test]
    fn a_stale_memory_revision_aborts_the_whole_commit() {
        let store = store();
        let memory = store
            .memory_repo()
            .insert_memory(&new_memory("m1", "tea"))
            .unwrap();
        let batch = batch(9, vec![context(&memory)]);
        // A concurrent correction moves the revision the pass captured.
        store
            .conn()
            .execute(
                "UPDATE memories SET memory_revision = memory_revision + 1 WHERE id = 'm1'",
                [],
            )
            .unwrap();
        let facts = vec![Fact {
            content: "green tea".into(),
            importance: Some(7),
            supersedes: Some("m1".into()),
            ..Fact::default()
        }];
        assert!(matches!(
            store
                .memory_repo()
                .consolidate(&batch, &facts)
                .map_err(HostError::from),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(
            store.memory_repo().memory("m1").unwrap().unwrap().content,
            "tea"
        );
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "the conflict rolled the whole pass back");
    }

    #[test]
    fn a_pass_merges_cites_and_writes_the_edges() {
        let store = store();
        let memory = store
            .memory_repo()
            .insert_memory(&new_memory("m2", "coffee"))
            .unwrap();
        let mut batch = batch(8, vec![context(&memory)]);
        batch.injected = vec!["m2".into()];
        let facts = vec![Fact {
            content: "A prefers coffee".into(),
            importance: Some(7),
            supersedes: Some("m2".into()),
            cited_memory_ids: vec!["m2".into(), "ghost".into()],
            ..Fact::default()
        }];
        let commit = store.memory_repo().consolidate(&batch, &facts).unwrap();
        assert_eq!((commit.inserted, commit.updated), (0, 1));
        let updated = store.memory_repo().memory("m2").unwrap().unwrap();
        assert_eq!(updated.content, "A prefers coffee");
        assert_eq!(updated.importance, 7);
        assert!(
            store
                .memory_repo()
                .memory_edges("m2")
                .unwrap()
                .iter()
                .any(|(relation, to)| relation == "estimated_cited_in" && to == "8"),
            "an accepted citation is filed against the window's newest message"
        );
        let citations: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE relation = 'estimated_cited_in'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(citations, 1, "the ghost citation had no injected row");
    }

    // === The pass end to end: retrieval, extraction, guard and commit ===

    fn companion() -> crate::host::mind::Companion {
        use crate::host::mind::{ModelAssignment, ModelAssignments};
        use ene_mind::JsonDoc;
        let assignment = ModelAssignment {
            connection_id: "c1".into(),
            model_id: "test-model".into(),
        };
        crate::host::mind::Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "kind".into(),
            profile: Default::default(),
            profile_revision: 1,
            state: Default::default(),
            state_revision: 1,
            body: JsonDoc::empty(),
            voice: JsonDoc::empty(),
            assignments: ModelAssignments {
                companion: assignment.clone(),
                task: assignment.clone(),
                consolidation: Some(assignment),
                ..Default::default()
            },
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// A store with one LLM connection and the companion's consolidation
    /// assignment, which is what the pass resolves its model through.
    fn configured(base: &str) -> Store {
        use crate::host::settings::{
            Connection, ConnectionConfig, ConnectionKind, CredentialState, Endpoint, LlmConnection,
        };
        let store = store();
        let revision = store.settings_repo().settings_revision().unwrap();
        store
            .settings_repo()
            .update_settings(
                revision,
                &[(
                    Connection {
                        id: ene_domain::ConnectionId::parse("c1").unwrap(),
                        name: "fixture".into(),
                        enabled: true,
                        kind: ConnectionKind::Llm,
                        config: ConnectionConfig::Llm(LlmConnection {
                            endpoint: Endpoint {
                                adapter: "openrouter".into(),
                                endpoint: base.into(),
                                options_schema: String::new(),
                                options: String::new(),
                            },
                            model_ids: vec!["test-model".into()],
                        }),
                        credential_state: CredentialState::Registered,
                        revision: 1,
                        last_check: None,
                    },
                    0,
                )],
            )
            .unwrap();
        store
            .companion_repo()
            .insert_companion(&companion())
            .unwrap();
        store
    }

    struct Setup {
        store: Store,
        credentials: Credentials,
        embeddings: Embeddings,
        models: Models,
        providers: crate::host::providers::Providers,
    }

    fn setup(base: &str) -> Setup {
        let store = configured(base);
        let credentials = Credentials::new(true);
        credentials.remember("c1", "test-key".into());
        Setup {
            store,
            credentials,
            embeddings: Embeddings::default(),
            models: Models::default(),
            providers: crate::host::providers::Providers::none(),
        }
    }

    fn services(setup: &Setup) -> Services {
        Services {
            store: setup.store.clone(),
            embeddings: setup.embeddings.clone(),
            credentials: Some(setup.credentials.clone()),
            models: setup.models.clone(),
            providers: setup.providers.clone(),
        }
    }

    /// A local OpenAI-compatible endpoint that answers one extraction call
    /// with `output`.
    struct ExtractionServer {
        base: String,
        task: tokio::task::JoinHandle<()>,
    }

    async fn serve_extraction(output: serde_json::Value) -> ExtractionServer {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let Ok(read) = socket.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&buffer[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break;
                }
            }
            let arguments = output.to_string();
            let body = serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": { "name": "submit", "arguments": arguments }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })
            .to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            // A write that fails means the batch never reached the peer; the
            // caller turns this into a retry, so the reason has to survive.
            ene_domain::warn_if_err(
                socket.write_all(head.as_bytes()).await,
                "recall header write",
            );
            ene_domain::warn_if_err(socket.write_all(body.as_bytes()).await, "recall body write");
            ene_domain::warn_if_err(socket.flush().await, "recall request flush");
        });
        ExtractionServer { base, task }
    }

    /// The normal path: the pass retrieves a non-empty context, the model
    /// supersedes the existing memory, and the commit stamps the window. An
    /// empty context would insert a second row instead.
    #[tokio::test]
    async fn the_pass_commits_the_context_it_retrieved_and_updates_the_memory() {
        let server = serve_extraction(serde_json::json!({ "facts": fact() })).await;
        let setup = setup(&server.base);
        let existing = setup
            .store
            .memory_repo()
            .insert_memory(&new_memory("m1", "東京都の紅茶の店"))
            .unwrap();
        let mut conversation = CoreConversation::new();
        answer(&mut conversation, "紅茶の店はどこ", "新宿にある");
        let commit = pass(
            &services(&setup),
            "ene",
            &conversation,
            None,
            newest(&conversation),
        )
        .await
        .unwrap();
        assert_eq!((commit.inserted, commit.updated), (0, 1));
        let updated = setup
            .store
            .memory_repo()
            .memory(&existing.id)
            .unwrap()
            .unwrap();
        assert_eq!(updated.content, "紅茶の店は新宿にある");
        assert_eq!(updated.importance, 7);
        let count: i64 = setup
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "the superseded memory is updated, not inserted again"
        );
        server.task.abort();
    }

    /// The reflection the interval left lands in the companion through the
    /// mind-update path, and the parts it does not name are untouched.
    #[tokio::test]
    async fn the_pass_applies_the_emotion_and_growth_the_interval_moved() {
        let server = serve_extraction(serde_json::json!({
            "facts": [],
            "state": { "emotion": { "mood": "gentle" } }
        }))
        .await;
        let setup = setup(&server.base);
        let mut conversation = CoreConversation::new();
        answer(&mut conversation, "一緒に紅茶を飲もう", "いいね");
        let commit = pass(
            &services(&setup),
            "ene",
            &conversation,
            None,
            newest(&conversation),
        )
        .await
        .unwrap();
        assert_eq!((commit.inserted, commit.updated), (0, 0));
        let after = setup
            .store
            .companion_repo()
            .companion("ene")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state.emotion.json,
            serde_json::json!({ "mood": "gentle" })
        );
        assert_eq!(
            after.state_revision, 2,
            "the reflection moved the mind once"
        );
        assert_eq!(
            after.state.relationships.json,
            serde_json::json!([]),
            "a part the reflection did not name keeps its value"
        );
        server.task.abort();
    }

    /// A mind edit that landed while the pass ran wins: the reflection of the
    /// window is discarded instead of overwriting it, and no receipt claims it
    /// was applied.
    #[test]
    fn a_concurrent_mind_edit_wins_over_the_reflection() {
        let setup = setup("http://127.0.0.1:9");
        let stale = setup
            .store
            .companion_repo()
            .companion("ene")
            .unwrap()
            .unwrap();
        setup
            .store
            .conn()
            .execute(
                "UPDATE companions SET state_revision = state_revision + 1 WHERE id = 'ene'",
                [],
            )
            .unwrap();
        let moved = setup
            .store
            .companion_repo()
            .companion("ene")
            .unwrap()
            .unwrap();
        assert_ne!(moved.state_revision, stale.state_revision);
        reflect(
            &services(&setup),
            &stale,
            9,
            Some(StateUpdate {
                emotion: Some(serde_json::json!({ "mood": "warm" })),
                relationships: None,
                tendencies: None,
                concerns: None,
            }),
        )
        .unwrap();
        let after = setup
            .store
            .companion_repo()
            .companion("ene")
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state_revision, moved.state_revision,
            "the concurrent edit keeps its revision"
        );
        assert_eq!(after.state, moved.state, "the reflection was discarded");
        let receipts: i64 = setup
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM mind_operations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(receipts, 0, "a discarded reflection leaves no receipt");
    }

    #[tokio::test]
    async fn direct_extractor_preserves_the_entire_request_and_cache_defaults() {
        use rig_core::test_utils::{MockCompletionModel, MockTurn};
        use rig_core::{catalog::Catalog, completion::CacheRetention};

        let mut spec = Catalog::builtin()
            .resolve("openai/gpt-4o-mini")
            .unwrap()
            .shared();
        std::sync::Arc::make_mut(&mut spec).caching.retention = vec![CacheRetention::Short];
        let mock = MockCompletionModel::from_turns(
            (0..2)
                .map(|_| MockTurn::tool_call("facts", "submit", serde_json::json!({"facts": []}))),
        )
        .priced_as(spec);
        let model: DynModel<Completion> = mock.clone().into();
        let options = model.spec().unwrap().default_options(None);
        assert_eq!(options.cache, Some(CacheRetention::Short));
        ExtractorBuilder::<Extraction>::from_agent_builder(
            rig_agent::agent::AgentBuilder::new(model.clone()).options(options),
        )
        .append_preamble(EXTRACTION_SYSTEM_PROMPT)
        .retries(0)
        .build()
        .extract("The user prefers tea.")
        .await
        .unwrap();
        assert_eq!(
            extract(&model, "The user prefers tea.".into())
                .await
                .unwrap()
                .facts,
            [] as [ene_mind::Fact; 0]
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert!(
            requests[1]
                .system_instructions()
                .unwrap()
                .contains(EXTRACTION_SYSTEM_PROMPT)
        );
        assert_eq!(requests[1].options.cache, Some(CacheRetention::Short));
    }

    #[tokio::test]
    async fn extraction_submits_bounded_facts_and_rejects_free_text() {
        use rig_core::test_utils::{MockCompletionModel, MockTurn};
        let model: DynModel<Completion> = MockCompletionModel::from_turns([
            MockTurn::tool_call(
                "facts",
                "submit",
                serde_json::json!({
                    "facts": [{"content": "  prefers tea  ", "importance": 7,
                        "scope": "", "supersedes": null, "cited_memory_ids": ["injected", "ghost"]}]
                }),
            ),
            MockTurn::text("```json\n{\"facts\": []}\n```"),
        ])
        .into();
        let extraction = extract(&model, "The user prefers tea.".into())
            .await
            .unwrap();
        let facts = extraction.facts;
        assert_eq!(facts[0].content, "prefers tea");
        assert_eq!(facts[0].importance, Some(7));
        assert_eq!(
            validate_citations(&facts[0].cited_memory_ids, &["injected".into()]),
            ["injected"]
        );
        assert!(extraction.state.is_none(), "nothing moved the mind");
        assert!(extract(&model, "No facts.".into()).await.is_err());
    }
}
