use super::*;

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
fn consolidation_does_not_share_a_private_correction_or_a_revoked_duplicate() {
    let store = store();
    companion_row(&store);
    let repo = store.memory_repo();
    let private = repo
        .insert_memory(&new_memory("the old private answer", 7))
        .unwrap();
    let mut other = new_memory("a previously shared fact", 7);
    other.scope = "shared".into();
    repo.insert_memory(&other).unwrap();
    repo.control_memory(&other.id, MemoryActionKind::Unshare, "other")
        .unwrap();
    let context = repo
        .consolidation_context("ene", "private answer", &[], 5)
        .unwrap();
    let facts = [
        Fact {
            content: "the corrected private answer".into(),
            scope: Some(SHARED_SCOPE.into()),
            supersedes: Some(private.id),
            ..Fact::default()
        },
        Fact {
            content: other.content,
            scope: Some(SHARED_SCOPE.into()),
            ..Fact::default()
        },
    ];
    let commit = repo.consolidate(&batch(7, context), &facts).unwrap();
    assert_eq!(commit.inserted, 1);
    assert_eq!(
        repo.list_memories("unrelated", None, 5).unwrap().0,
        [] as [Memory; 0]
    );
    let (entries, _) = repo.list_memories("ene", None, 5).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|memory| memory.content.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from([
            "the corrected private answer",
            "the old private answer"
        ])
    );
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
