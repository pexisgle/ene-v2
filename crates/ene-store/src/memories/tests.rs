mod consolidation;
mod recall;

use super::*;
use serde_json::Value;

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
        &version.to_le_bytes(),
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
    let hash = b"tea";
    let write = input("tea", 5);
    let result = apply(&store, "op-1", hash, vec![write.clone()], None).unwrap();
    let id = result["memories"][0].as_str().unwrap().to_owned();
    assert_eq!(
        apply(&store, "op-1", hash, vec![write], None).unwrap(),
        result
    );
    assert_eq!(repo.list_memories("ene", None, 10).unwrap().0.len(), 1);
    assert!(matches!(
        apply(&store, "op-1", b"coffee", vec![input("coffee", 5)], None),
        Err(StoreError::Conflict(_))
    ));
    let correction = apply(
        &store,
        "op-2",
        b"green tea",
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
            b"black tea",
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
    let corrected_id = correction["memories"][0].as_str().unwrap().to_owned();
    assert_preserved_correction(repo, &id, &corrected_id);
    apply(
        &store,
        "op-4",
        b"forget",
        vec![NewMemory {
            id: Some(corrected_id.clone()),
            forget: true,
            expected_revision: Some(1),
            ..NewMemory::default()
        }],
        None,
    )
    .unwrap();
    let forgotten = repo.memory(&corrected_id).unwrap().unwrap();
    assert_eq!(forgotten.importance, 0);
    assert_eq!(forgotten.memory_revision, 2);
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
fn a_memory_change_needs_one_target_at_its_observed_revision() {
    let store = store();
    let repo = store.memory_repo();
    companion_row(&store);
    let old = repo.insert_memory(&new_memory("tea", 5)).unwrap();
    let other = repo.insert_memory(&new_memory("coffee", 4)).unwrap();
    let revise = NewMemory {
        supersedes: Some(old.id.clone()),
        content: "green tea".into(),
        importance: Some(7),
        ..NewMemory::default()
    };
    let hash = b"green tea";
    for edit in [
        revise.clone(),
        NewMemory {
            id: Some(other.id.clone()),
            expected_revision: Some(old.memory_revision),
            ..revise.clone()
        },
        NewMemory {
            id: Some(other.id.clone()),
            forget: true,
            ..NewMemory::default()
        },
    ] {
        assert!(matches!(
            apply(
                &store,
                "rejected",
                hash,
                vec![input("rolled back", 3), edit],
                None
            ),
            Err(StoreError::Invalid(_))
        ));
    }
    assert_eq!(repo.list_memories("ene", None, 10).unwrap().0.len(), 2);
    assert_eq!(repo.memory(&old.id).unwrap().unwrap(), old);
    assert_eq!(repo.memory(&other.id).unwrap().unwrap(), other);
    let result = apply(
        &store,
        "accepted",
        hash,
        vec![NewMemory {
            expected_revision: Some(old.memory_revision),
            ..revise
        }],
        None,
    )
    .unwrap();
    assert_preserved_correction(repo, &old.id, result["memories"][0].as_str().unwrap());
}

fn assert_preserved_correction(repo: MemoryRepo<'_>, previous: &str, corrected_id: &str) {
    assert_ne!(corrected_id, previous);
    let original = repo.memory(previous).unwrap().unwrap();
    assert_eq!(original.content, "tea");
    assert_eq!(original.importance, 2);
    assert_eq!(original.memory_revision, 2);
    let corrected = repo.memory(corrected_id).unwrap().unwrap();
    assert_eq!(corrected.content, "green tea");
    assert_eq!(corrected.importance, 7);
    assert_eq!(corrected.memory_revision, 1);
    assert!(
        repo.memory_edges(corrected_id)
            .unwrap()
            .contains(&("supersedes".into(), previous.into()))
    );
    let context = repo
        .consolidation_context("ene", "green tea", &[], 5)
        .unwrap();
    assert_eq!(
        repo.consolidate(
            &batch(8, context),
            &[Fact {
                content: "The tea was counterfeit".into(),
                supersedes: Some(corrected_id.into()),
                ..Fact::default()
            }]
        )
        .unwrap()
        .inserted,
        0
    );
}
