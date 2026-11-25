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
fn recurring_cached_embedding_writes_keep_bindings_and_revision_guards() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let repo = store.memory_repo();
    let first = repo.insert_memory(&new_memory("first", 3)).unwrap();
    let second = repo.insert_memory(&new_memory("second", 4)).unwrap();
    let connection = store.open_secondary().unwrap();
    let a = 1.0_f32.to_le_bytes();
    let b = 2.0_f32.to_le_bytes();
    for (memory, bytes) in [(&first, &a), (&second, &b), (&first, &b)] {
        assert!(
            write_embedding_on(
                &connection,
                &memory.id,
                memory.content_revision,
                "fake:1:v1",
                1,
                bytes
            )
            .unwrap()
        );
    }
    assert!(
        !write_embedding_on(
            &connection,
            &first.id,
            first.content_revision + 1,
            "fake:1:v1",
            1,
            &a
        )
        .unwrap()
    );
    assert!(
        write_embedding_on(
            &connection,
            &second.id,
            second.content_revision,
            "fake:1:v1",
            1,
            &a
        )
        .unwrap()
    );
    assert_eq!(
        repo.embedding_blob(&first.id, "fake:1:v1").unwrap(),
        Some((1, b.to_vec()))
    );
    assert_eq!(
        repo.embedding_blob(&second.id, "fake:1:v1").unwrap(),
        Some((1, a.to_vec()))
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
            .unwrap()
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
            &[semantic_only.id.clone(), lexical.id.clone()],
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
            std::slice::from_ref(&lexical.id),
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

fn companion_row(store: &Store) {
    let initial_state = serde_json::to_string(&ene_mind::State::default()).unwrap();
    let document = r#"{"schema":"ene.body.v1","json":{}}"#;
    let assignments = r#"{"companion":{"connection_id":"c","model_id":"m"},"task":{"connection_id":"c","model_id":"m"}}"#;
    store
        .conn()
        .unwrap()
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
            state: serde_json::to_string(&ene_mind::State::default()).unwrap(),
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
            std::slice::from_ref(&other.id),
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
            std::slice::from_ref(&other.id),
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
        through,
        context,
        injected: Vec::new(),
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

    let commit = repo
        .consolidate(
            &batch(9, Vec::new()),
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
fn injected_memory_ids_survive_a_restart() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.sqlite3");
    {
        let store = Store::open(&path).unwrap();
        let repo = store.memory_repo();
        repo.record_injections("ene", "5", &["m1".into(), "m2".into()])
            .unwrap();
        repo.record_injections("ene", "7", &["m3".into()]).unwrap();
        repo.record_injections("other", "5", &["m9".into()])
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let repo = store.memory_repo();
    assert_eq!(
        repo.injected_between("ene", Some(5), 7).unwrap(),
        vec!["m3".to_owned()]
    );
    assert_eq!(
        repo.injected_between("ene", None, 7).unwrap(),
        vec!["m1".to_owned(), "m2".to_owned(), "m3".to_owned()]
    );
    assert!(
        repo.injected_between("ene", Some(5), 5).unwrap().is_empty(),
        "after is exclusive, so an empty range has no ids"
    );
    assert_eq!(
        repo.injected_between("other", None, 5).unwrap(),
        vec!["m9".to_owned()]
    );
}
