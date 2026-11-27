use super::*;
use crate::util::digest;
use serde_json::json as value;

fn store() -> Store {
    let temp = tempfile::tempdir().unwrap();
    Store::open(&temp.path().join("host.sqlite3")).unwrap()
}

fn new_memory(content: &str, importance: u8) -> MemoryRecord {
    MemoryRecord {
        id: uuid::Uuid::new_v4().to_string(),
        scope: "ene".into(),
        content: content.into(),
        importance,
        occurred_at: None,
        source_context: serde_json::json!({}),
    }
}

fn input(content: &str, importance: u8) -> NewMemory {
    NewMemory {
        content: content.into(),
        importance: Some(importance),
        ..NewMemory::default()
    }
}

fn apply(
    store: &Store,
    operation: &str,
    hash: &[u8],
    memories: Vec<NewMemory>,
    state: Option<MindStateChange>,
) -> Result<Value> {
    store.memory_repo().apply_mind_update(&MindUpdate {
        operation_id: operation.into(),
        companion_id: "ene".into(),
        request_hash: hash.to_vec(),
        memories,
        state,
    })
}

#[test]
fn embedding_spaces_share_current_content_without_overwrites() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let repo = store.memory_repo();
    let first = repo.insert_memory(&new_memory("first", 3)).unwrap();
    let second = repo.insert_memory(&new_memory("second", 4)).unwrap();
    repo.control_memory(&first.id, MemoryActionKind::Share, "ene")
        .unwrap();
    let secondary = store.open_secondary().unwrap();
    let writer = secondary.memory_repo();
    let first = RecallCandidate::from(&first);
    let second = RecallCandidate::from(&second);
    let a = 1.0_f32.to_le_bytes();
    let b = 2.0_f32.to_le_bytes();
    let space_a = "alpha:1:v1";
    let space_b = "beta:1:v1";
    for (companion, candidate, space, bytes) in [
        ("ene", &first, space_a, &a),
        ("ene", &second, space_a, &b),
        ("other", &first, space_b, &b),
    ] {
        assert!(
            writer
                .write_embedding(companion, candidate, space, bytes)
                .unwrap()
        );
    }
    assert!(!writer.write_embedding("ene", &first, space_a, &b).unwrap());
    assert!(
        !writer
            .write_embedding("other", &second, space_b, &a)
            .unwrap()
    );
    assert_eq!(
        repo.embedding_blob(&first, space_a).unwrap().as_deref(),
        Some(&a[..])
    );
    assert_eq!(
        repo.embedding_blob(&first, space_b).unwrap().as_deref(),
        Some(&b[..])
    );
    assert_eq!(
        repo.embedding_blob(&second, space_a).unwrap().as_deref(),
        Some(&b[..])
    );
    assert_eq!(
        repo.memories_missing_embedding("other", space_b, 10)
            .unwrap(),
        Vec::<PendingEmbedding>::new()
    );

    repo.control_memory(&first.id, MemoryActionKind::Unshare, "ene")
        .unwrap();
    assert!(
        !writer
            .write_embedding("other", &first, "third:1:v1", &a)
            .unwrap()
    );
    store.conn().execute(
        "UPDATE memories SET content = 'corrected', content_revision = content_revision + 1 WHERE id = ?1",
        [&first.id],
    ).unwrap();
    for space in [space_a, space_b] {
        assert_eq!(repo.embedding_blob(&first, space).unwrap(), None);
        assert!(!writer.write_embedding("ene", &first, space, &a).unwrap());
        let pending = repo.memories_missing_embedding("ene", space, 10).unwrap();
        assert_eq!(pending[0].candidate.id, first.id);
        assert_eq!(
            pending[0].candidate.content_revision,
            first.content_revision + 1
        );
        assert!(
            writer
                .write_embedding("ene", &pending[0].candidate, space, &a)
                .unwrap()
        );
    }
    store.reset_learned_state().unwrap();
    assert_eq!(
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn historical_hash_receipts_replay_after_reopening() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.sqlite3");
    let store = Store::open(&path).unwrap();
    let repo = store.memory_repo();
    let fixtures = [
        ("float", r#"{"n":1.0}"#),
        ("unicode", "{\"\u{e000}\":1,\"\u{10000}\":2}"),
    ];
    for (operation, historical) in fixtures {
        let request: Value = serde_json::from_str(historical).unwrap();
        let legacy_hash = digest(historical.as_bytes());
        assert_eq!(repo.mind_hash(&request), legacy_hash);
        let canonical = serde_json_canonicalizer::to_string(&request).unwrap();
        assert_ne!(digest(canonical.as_bytes()), legacy_hash);
        store
            .conn()
            .execute(
                "INSERT INTO mind_operations (operation_id, companion_id, request_hash, result, created_at) \
                 VALUES (?1, 'ene', ?2, '{\"replayed\":true}', ?3)",
                params![operation, legacy_hash, now()],
            )
            .unwrap();
    }
    drop(store);
    let store = Store::open(&path).unwrap();
    let repo = store.memory_repo();
    for (operation, historical) in fixtures {
        let request: Value = serde_json::from_str(historical).unwrap();
        assert_eq!(
            apply(
                &store,
                operation,
                &repo.mind_hash(&request),
                Vec::new(),
                None
            )
            .unwrap(),
            value!({"replayed": true})
        );
        let canonical = serde_json_canonicalizer::to_string(&request).unwrap();
        assert!(matches!(
            apply(
                &store,
                operation,
                &digest(canonical.as_bytes()),
                Vec::new(),
                None
            ),
            Err(StoreError::Conflict(_))
        ));
    }
}

#[test]
fn hybrid_recall_fuses_and_forgetting_excludes_auto_but_keeps_explicit() {
    let store = store();
    let repo = store.memory_repo();
    let lexical = repo
        .insert_memory(&new_memory("東京都の紅茶の店", 6))
        .unwrap();
    let semantic_only = repo
        .insert_memory(&new_memory("a favourite drink", 5))
        .unwrap();
    let outcome = repo
        .recall_hybrid(
            "ene",
            "紅茶",
            RecallMode::Auto,
            &[
                RecallCandidate::from(&semantic_only),
                RecallCandidate::from(&lexical),
            ],
            8,
        )
        .unwrap();
    assert!(outcome.semantic);
    assert_eq!(outcome.items.len(), 2);
    assert!(
        outcome
            .items
            .iter()
            .any(|item| item.memory.id == semantic_only.id)
    );
    let lexical_only = repo
        .recall_hybrid("ene", "紅茶", RecallMode::Auto, &[], 8)
        .unwrap();
    assert!(!lexical_only.semantic);
    assert_eq!(lexical_only.items.len(), 1);

    repo.control_memory(&lexical.id, MemoryActionKind::Forget, "ene")
        .unwrap();
    assert_eq!(repo.memory(&lexical.id).unwrap().unwrap().importance, 0);
    assert_eq!(
        repo.recall_hybrid(
            "ene",
            "紅茶",
            RecallMode::Auto,
            &[RecallCandidate::from(&lexical)],
            4,
        )
        .unwrap()
        .items
        .len(),
        0
    );
    assert_eq!(
        repo.recall_hybrid("ene", "紅茶", RecallMode::Explicit, &[], 4)
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn semantic_retrieval_restores_only_current_revisions_in_candidate_order() {
    let store = store();
    let repo = store.memory_repo();
    let first = repo.insert_memory(&new_memory("first", 4)).unwrap();
    let second = repo.insert_memory(&new_memory("second", 4)).unwrap();
    let stale = RecallCandidate::from(&first);
    store
        .conn()
        .execute(
            "UPDATE memories SET content = 'corrected', content_revision = content_revision + 1 \
         WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
    let corrected = repo.memory(&first.id).unwrap().unwrap();
    let second_candidate = RecallCandidate::from(&second);
    let candidates = [
        stale,
        second_candidate.clone(),
        second_candidate,
        (&corrected).into(),
    ];
    assert_eq!(
        repo.semantic_memories("ene", RecallMode::Auto, &candidates)
            .unwrap(),
        [second, corrected]
    );
}

#[test]
fn lexical_recall_handles_short_terms_and_literal_wildcards() {
    let store = store();
    let repo = store.memory_repo();
    let dog = repo
        .insert_memory(&new_memory("犬 café 東京都", 4))
        .unwrap();
    let ai = repo.insert_memory(&new_memory("Uses AI _%", 4)).unwrap();
    for (query, count) in [
        ("犬、ai", 2),
        ("café", 1),
        ("東京都", 1),
        ("_%", 1),
        ("  ", 0),
        ("！", 0),
    ] {
        assert_eq!(
            repo.lexical_candidates("ene", query, RecallMode::Auto)
                .unwrap()
                .len(),
            count,
            "{query:?}"
        );
    }
    assert_eq!(
        repo.lexical_candidates("ene", "_%", RecallMode::Auto)
            .unwrap(),
        [ai]
    );
    assert_eq!(
        repo.lexical_candidates("ene", "東京都", RecallMode::Auto)
            .unwrap(),
        [dog]
    );
}

#[test]
fn lexical_ties_and_repeated_query_terms_keep_the_same_top_twenty() {
    let store = store();
    let repo = store.memory_repo();
    let rows = (0..21)
        .map(|_| {
            repo.insert_memory(&new_memory("frequent marker", 4))
                .unwrap()
        })
        .collect::<Vec<_>>();
    for query in [
        "frequent marker".to_owned(),
        "frequent ".repeat(200) + "marker",
    ] {
        assert_eq!(
            repo.lexical_candidates("ene", &query, RecallMode::Auto)
                .unwrap(),
            rows[..20]
        );
    }
}

fn companion_row(store: &Store) {
    let initial_state = serde_json::to_string(&ene_mind::State::default()).unwrap();
    let document = r#"{"schema":"ene.body.v1","json":{}}"#;
    let assignments = r#"{"companion":{"connection_id":"c","model_id":"m"},"task":{"connection_id":"c","model_id":"m"}}"#;
    store
        .conn()
        .execute(
            "INSERT INTO companions \
             (id, name, mind, profile, state, body, voice, assignments, created_at, updated_at) \
             VALUES ('ene', 'ene', 'core', '{}', ?1, ?2, ?2, ?3, 'now', 'now')",
            params![initial_state, document, assignments],
        )
        .unwrap();
}

fn state_only(store: &Store, operation: &str, version: i64) -> Result<Value> {
    apply(
        store,
        operation,
        &store.memory_repo().mind_hash(&value!({"state": version})),
        Vec::new(),
        Some(MindStateChange {
            expected_revision: 1,
            state: ene_mind::State::default(),
        }),
    )
}

#[test]
fn mind_update_is_idempotent_and_corrections_forgets_and_state_use_cas() {
    let store = store();
    let repo = store.memory_repo();
    let companions = store.companion_repo();
    companion_row(&store);
    let hash = repo.mind_hash(&value!({"content": "tea", "v": 1}));
    let write = input("tea", 5);
    let result = apply(&store, "op-1", &hash, vec![write.clone()], None).unwrap();
    let id = result["memories"][0].as_str().unwrap().to_owned();
    assert_eq!(
        apply(&store, "op-1", &hash, vec![write], None).unwrap(),
        result
    );
    assert_eq!(repo.list_memories("ene", None, 10).unwrap().0.len(), 1);
    assert!(matches!(
        apply(
            &store,
            "op-1",
            &repo.mind_hash(&value!({"content": "coffee"})),
            vec![input("coffee", 5)],
            None
        ),
        Err(StoreError::Conflict(_))
    ));
    apply(
        &store,
        "op-2",
        &repo.mind_hash(&value!({"content": "green tea"})),
        vec![NewMemory {
            id: Some(id.clone()),
            content: "green tea".into(),
            importance: Some(7),
            expected_revision: Some(1),
            ..NewMemory::default()
        }],
        None,
    )
    .unwrap();
    assert!(matches!(
        apply(
            &store,
            "op-3",
            &repo.mind_hash(&value!({"content": "black tea"})),
            vec![NewMemory {
                id: Some(id.clone()),
                content: "black tea".into(),
                expected_revision: Some(1),
                ..NewMemory::default()
            }],
            None,
        ),
        Err(StoreError::Conflict(_))
    ));
    let corrected = repo.memory(&id).unwrap().unwrap();
    assert_eq!(corrected.content, "green tea");
    assert_eq!(corrected.importance, 7);
    assert_eq!(corrected.memory_revision, 2);
    apply(
        &store,
        "op-4",
        &repo.mind_hash(&value!({"forget": 1})),
        vec![NewMemory {
            id: Some(id.clone()),
            forget: true,
            expected_revision: Some(2),
            ..NewMemory::default()
        }],
        None,
    )
    .unwrap();
    let forgotten = repo.memory(&id).unwrap().unwrap();
    assert_eq!(forgotten.importance, 0);
    assert_eq!(forgotten.memory_revision, 3);
    state_only(&store, "op-5", 1).unwrap();
    assert_eq!(
        companions.companion("ene").unwrap().unwrap().state_revision,
        2
    );
    assert!(matches!(
        state_only(&store, "op-6", 2),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        companions.companion("ene").unwrap().unwrap().state_revision,
        2
    );
}

#[test]
fn another_companions_scope_is_private_and_sharing_moves_the_row_to_the_pool() {
    let store = store();
    let repo = store.memory_repo();
    let mut other = new_memory("other companion's secret", 8);
    other.scope = "other".into();
    let other = repo.insert_memory(&other).unwrap();
    let private = repo
        .recall_hybrid(
            "ene",
            "secret",
            RecallMode::Explicit,
            &[RecallCandidate::from(&other)],
            8,
        )
        .unwrap();
    assert_eq!(private.items.len(), 0);
    let shared = repo
        .control_memory(&other.id, MemoryActionKind::Share, "other")
        .unwrap();
    assert_eq!(shared.scope, SHARED_SCOPE);
    assert_eq!(shared.memory_revision, 2);
    assert_eq!(
        repo.control_memory(&other.id, MemoryActionKind::Share, "other")
            .unwrap()
            .memory_revision,
        2
    );
    let recalled = repo
        .recall_hybrid(
            "ene",
            "secret",
            RecallMode::Explicit,
            &[RecallCandidate::from(&other)],
            8,
        )
        .unwrap();
    assert_eq!(recalled.items.len(), 1);
    assert_eq!(recalled.items[0].memory.id, other.id);
    let unshared = repo
        .control_memory(&other.id, MemoryActionKind::Unshare, "other")
        .unwrap();
    assert_eq!(unshared.scope, "other");
    assert_eq!(unshared.memory_revision, 3);
    assert_eq!(
        repo.recall_hybrid("ene", "secret", RecallMode::Explicit, &[], 8)
            .unwrap()
            .items
            .len(),
        0
    );
}

#[test]
fn memory_pages_include_the_lookahead_row_on_the_next_page() {
    let store = store();
    let repo = store.memory_repo();
    let first = repo.insert_memory(&new_memory("first", 4)).unwrap();
    let mut hidden = new_memory("different scope", 4);
    hidden.scope = "other".into();
    repo.insert_memory(&hidden).unwrap();
    let second = repo.insert_memory(&new_memory("second", 4)).unwrap();
    let third = repo.insert_memory(&new_memory("third", 4)).unwrap();

    let (page, cursor) = repo.list_memories("ene", None, 1).unwrap();
    assert_eq!(page, [first]);
    let (page, cursor) = repo.list_memories("ene", cursor, 1).unwrap();
    assert_eq!(page, [second]);
    let (page, cursor) = repo.list_memories("ene", cursor, 1).unwrap();
    assert_eq!(page, [third]);
    assert_eq!(cursor, None);
}

#[test]
fn pinned_memory_that_exceeds_the_budget_leaves_room_for_the_next_row() {
    let store = store();
    let repo = store.memory_repo();
    let small = repo.insert_memory(&new_memory("短い記憶", 10)).unwrap();
    let oversized = "長".repeat(PINNED_BUDGET_CHARS + 1);
    repo.insert_memory(&new_memory(&oversized, 10)).unwrap();
    assert_eq!(repo.pinned("ene").unwrap(), [small]);
}

#[test]
fn a_shared_pinned_memory_is_injected_for_every_companion() {
    let store = store();
    let repo = store.memory_repo();
    let memory = repo
        .insert_memory(&new_memory("the user's name is A", 10))
        .unwrap();
    assert_eq!(repo.pinned("ene").unwrap().len(), 1);
    assert_eq!(repo.pinned("other").unwrap().len(), 0);
    repo.control_memory(&memory.id, MemoryActionKind::Share, "ene")
        .unwrap();
    assert_eq!(repo.pinned("ene").unwrap().len(), 1);
    assert_eq!(repo.pinned("other").unwrap().len(), 1);
    assert_eq!(repo.list_memories("other", None, 10).unwrap().0.len(), 1);
}

fn batch(through: u64, context: Vec<MemoryContext>) -> Batch {
    Batch {
        companion_id: "ene".into(),
        companion_created_at: "now".into(),
        after: None,
        checkpoint: ene_mind::Checkpoint {
            lineage: "fixture".into(),
            summary: ene_mind::Summary {
                text: "Summary".into(),
                through_message_id: through,
            },
        },
        context,
        injected: Vec::new(),
        reflection: None,
    }
}

#[test]
fn consolidation_context_retrieves_instead_of_ranking_importance() {
    let store = store();
    let repo = store.memory_repo();
    let relevant = repo
        .insert_memory(&new_memory("東京都の紅茶の店", 6))
        .unwrap();
    let important = repo
        .insert_memory(&new_memory("unrelated pinned fact", 10))
        .unwrap();
    let forgotten = repo
        .insert_memory(&new_memory("紅茶はもう忘れて", 1))
        .unwrap();
    let contexts = repo.consolidation_context("ene", "紅茶", &[], 2).unwrap();
    let ids: Vec<&str> = contexts.iter().map(|context| context.id.as_str()).collect();
    assert!(ids.contains(&relevant.id.as_str()));
    assert!(
        ids.contains(&forgotten.id.as_str()),
        "a matching forgotten memory is shown, so the model can avoid reviving it"
    );
    assert!(
        !ids.contains(&important.id.as_str()),
        "an unrelated importance-10 memory does not crowd out retrieval"
    );
    let fallback = repo.consolidation_context("ene", "zzz", &[], 2).unwrap();
    assert_eq!(fallback.len(), 2);
}

#[test]
fn consolidation_does_not_resurrect_a_forgotten_memory() {
    let store = store();
    companion_row(&store);
    let repo = store.memory_repo();
    let memory = repo.insert_memory(&new_memory("coffee", 5)).unwrap();
    repo.control_memory(&memory.id, MemoryActionKind::Forget, "ene")
        .unwrap();
    let forgotten = repo.memory(&memory.id).unwrap().unwrap();
    assert_eq!(forgotten.importance, 0);

    let fact = Fact {
        content: "A drinks coffee every morning".into(),
        importance: Some(9),
        supersedes: Some(memory.id.clone()),
        ..Fact::default()
    };
    let context = vec![MemoryContext {
        id: forgotten.id.clone(),
        revision: forgotten.memory_revision,
        importance: forgotten.importance,
        content: forgotten.content.clone(),
    }];
    let commit = repo.consolidate(&batch(7, context), &[fact]).unwrap();
    assert_eq!(commit.inserted, 0);
    assert_eq!(commit.updated, 0);
    let after = repo.memory(&memory.id).unwrap().unwrap();
    assert_eq!(after.importance, 0, "the forgotten row stays forgotten");
    assert_eq!(after.content, "coffee");

    let mut second = batch(9, Vec::new());
    second.after = Some(7);
    let commit = repo
        .consolidate(
            &second,
            &[Fact {
                content: "A drinks coffee every morning".into(),
                importance: Some(9),
                supersedes: Some(memory.id.clone()),
                ..Fact::default()
            }],
        )
        .unwrap();
    assert_eq!(commit.inserted, 0);
    assert_eq!(repo.memory(&memory.id).unwrap().unwrap().importance, 0);
}

#[test]
fn stale_consolidation_cannot_recreate_forgotten_text_or_use_revoked_context() {
    let store = store();
    companion_row(&store);
    let repo = store.memory_repo();
    let forgotten = repo
        .insert_memory(&new_memory("coffee every morning", 5))
        .unwrap();
    repo.control_memory(&forgotten.id, MemoryActionKind::Forget, "ene")
        .unwrap();
    let pinned = |content: &str| Fact {
        content: content.into(),
        importance: Some(10),
        ..Fact::default()
    };
    let commit = repo
        .consolidate(
            &batch(7, Vec::new()),
            &[
                pinned(" coffee every morning "),
                pinned("gardening at noon"),
            ],
        )
        .unwrap();
    assert_eq!(commit.inserted, 1);
    let pins = repo.pinned("ene").unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0].content, "gardening at noon");
    assert_eq!(repo.memory(&forgotten.id).unwrap().unwrap().importance, 0);

    let mut shared = new_memory("formerly shared", 4);
    shared.scope = "other".into();
    let shared = repo.insert_memory(&shared).unwrap();
    repo.control_memory(&shared.id, MemoryActionKind::Share, "other")
        .unwrap();
    let context = repo
        .consolidation_context("ene", "formerly shared", &[], 32)
        .unwrap();
    assert!(context.iter().any(|memory| memory.id == shared.id));
    repo.control_memory(&shared.id, MemoryActionKind::Unshare, "other")
        .unwrap();
    let mut second = batch(9, context);
    second.after = Some(7);
    assert!(matches!(
        repo.consolidate(&second, &[pinned("derived from stale context")]),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(repo.pinned("ene").unwrap().len(), 1);
}

#[test]
fn injected_memory_ids_survive_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.sqlite3");
    {
        let store = Store::open(&path).unwrap();
        let repo = store.memory_repo();
        for (id, scope) in [
            ("m1", "ene"),
            ("m2", "shared"),
            ("m3", "ene"),
            ("m9", "other"),
        ] {
            let mut memory = new_memory(id, 4);
            memory.id = id.into();
            memory.scope = scope.into();
            repo.insert_memory(&memory).unwrap();
        }
        let stamp = |id| MemoryVersion::from(&repo.memory(id).unwrap().unwrap());
        repo.record_injections("ene", "5", &[stamp("m1"), stamp("m2")])
            .unwrap();
        repo.record_injections("ene", "7", &[stamp("m3")]).unwrap();
        repo.record_injections("other", "5", &[stamp("m9")])
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let repo = store.memory_repo();
    let stamp = |id| MemoryVersion::from(&repo.memory(id).unwrap().unwrap());
    assert_eq!(
        repo.injected_between("ene", Some(5), 7).unwrap(),
        [stamp("m3")]
    );
    assert_eq!(
        repo.injected_between("ene", None, 7).unwrap(),
        [stamp("m1"), stamp("m2"), stamp("m3")]
    );
    assert!(
        repo.injected_between("ene", Some(5), 5).unwrap().is_empty(),
        "after is exclusive, so an empty range has no ids"
    );
    assert_eq!(
        repo.injected_between("other", None, 5).unwrap(),
        [stamp("m9")]
    );
}

#[test]
fn a_failed_citation_never_advances_the_memory_access_time() {
    let store = store();
    let repo = store.memory_repo();
    let memory = repo.insert_memory(&new_memory("test read", 4)).unwrap();
    store
        .conn()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_citation BEFORE INSERT ON memory_edges BEGIN
           SELECT RAISE(ABORT, 'citation fixture failure');
         END;",
        )
        .unwrap();
    assert!(repo.read_and_cite("ene", &memory.id, "head:5").is_err());
    assert_eq!(
        repo.memory(&memory.id).unwrap().unwrap().last_accessed_at,
        None
    );
    store
        .conn()
        .execute_batch("DROP TRIGGER fail_citation")
        .unwrap();
    assert!(matches!(
        repo.read_and_cite("other", &memory.id, "other:1"),
        Err(StoreError::Permission(_))
    ));
    let (read, edges) = repo.read_and_cite("ene", &memory.id, "head:5").unwrap();
    assert!(read.last_accessed_at.is_some());
    assert_eq!(edges, [("cited_in".into(), "head:5".into())]);
}

#[test]
fn a_memory_unshared_after_injection_cannot_be_cited_by_consolidation() {
    let store = store();
    companion_row(&store);
    let repo = store.memory_repo();
    let mut input = new_memory("shared note", 5);
    input.scope = SHARED_SCOPE.into();
    let memory = repo.insert_memory(&input).unwrap();
    let version = MemoryVersion::from(&memory);
    repo.record_injections("ene", "5", std::slice::from_ref(&version))
        .unwrap();
    let mut consolidation = batch(7, Vec::new());
    consolidation.injected = repo.injected_between("ene", None, 7).unwrap();
    assert_eq!(consolidation.injected, std::slice::from_ref(&version));
    repo.control_memory(&memory.id, MemoryActionKind::Unshare, "owner")
        .unwrap();
    assert_eq!(
        repo.injected_between("ene", None, 7).unwrap(),
        [] as [MemoryVersion; 0]
    );
    let fact = Fact {
        content: "a fact derived from the old note".into(),
        cited_memory_ids: vec![memory.id.clone()],
        ..Fact::default()
    };
    assert!(matches!(
        repo.consolidate(&consolidation, std::slice::from_ref(&fact)),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        repo.list_memories("ene", None, 10).unwrap().0,
        [] as [Memory; 0]
    );
    assert_eq!(
        repo.memory(&memory.id).unwrap().unwrap().last_accessed_at,
        None
    );
    assert_eq!(
        repo.memory_edges(&memory.id).unwrap(),
        [] as [(String, String); 0]
    );
    let shared = repo
        .control_memory(&memory.id, MemoryActionKind::Share, "owner")
        .unwrap();
    repo.record_injections("ene", "6", &[version]).unwrap();
    assert_eq!(
        repo.injected_between("ene", None, 7).unwrap(),
        [] as [MemoryVersion; 0]
    );
    assert!(matches!(
        repo.consolidate(&consolidation, std::slice::from_ref(&fact)),
        Err(StoreError::Conflict(_))
    ));
    let shared = MemoryVersion::from(&shared);
    repo.record_injections("ene", "7", std::slice::from_ref(&shared))
        .unwrap();
    consolidation.injected = repo.injected_between("ene", None, 7).unwrap();
    assert_eq!(consolidation.injected, [shared]);
    assert_eq!(
        repo.consolidate(&consolidation, &[fact]).unwrap().inserted,
        1
    );
    assert_eq!(
        repo.memory_edges(&memory.id).unwrap(),
        [("estimated_cited_in".into(), "7".into())]
    );
}

#[test]
fn failed_reflection_and_retired_owner_leave_no_consolidated_fact() {
    let store = store();
    companion_row(&store);
    let repo = store.memory_repo();
    let mut proposal = batch(7, Vec::new());
    proposal.reflection = Some(MindStateChange {
        expected_revision: 1,
        state: ene_mind::State::default(),
    });
    let facts = [Fact {
        content: "a new fact extracted before the failure".into(),
        scope: Some(SHARED_SCOPE.into()),
        ..Fact::default()
    }];
    store
        .conn()
        .execute_batch(
            "CREATE TEMP TRIGGER fail_reflection BEFORE UPDATE OF state ON companions BEGIN
         SELECT RAISE(ABORT, 'reflection fixture failure'); END;",
        )
        .unwrap();
    assert!(matches!(
        repo.consolidate(&proposal, &facts),
        Err(StoreError::Storage(_))
    ));
    assert_eq!(repo.compaction_checkpoint("ene").unwrap(), None);
    assert_eq!(
        repo.list_memories("any-companion", None, 9).unwrap().0,
        [] as [Memory; 0]
    );
    store
        .conn()
        .execute_batch("DROP TRIGGER fail_reflection")
        .unwrap();
    proposal.reflection.as_mut().unwrap().state.emotion.json = Value::Bool(true);
    assert!(matches!(
        repo.consolidate(&proposal, &facts),
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(
        repo.list_memories("any-companion", None, 9).unwrap().0,
        [] as [Memory; 0]
    );
    assert!(store.companion_repo().delete_companion("ene").unwrap());
    assert!(matches!(
        repo.consolidate(&proposal, &facts),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(
        repo.list_memories("any-companion", None, 9).unwrap().0,
        [] as [Memory; 0]
    );
}

#[test]
fn a_persisted_checkpoint_prevents_a_replayed_window_from_writing_again() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("host.sqlite3");
    let proposal = batch(7, Vec::new());
    let facts = [Fact {
        content: "a durable fact".into(),
        ..Fact::default()
    }];
    let store = Store::open(&path).unwrap();
    companion_row(&store);
    store.memory_repo().consolidate(&proposal, &facts).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    let repo = store.memory_repo();
    assert_eq!(
        repo.compaction_checkpoint("ene").unwrap(),
        Some(proposal.checkpoint.clone())
    );
    assert!(matches!(
        repo.consolidate(&proposal, &facts),
        Err(StoreError::Conflict(_))
    ));
    assert_eq!(repo.list_memories("ene", None, 9).unwrap().0.len(), 1);
    store.reset_learned_state().unwrap();
    assert_eq!(repo.compaction_checkpoint("ene").unwrap(), None);
}
