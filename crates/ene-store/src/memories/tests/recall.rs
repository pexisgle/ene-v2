use super::*;
use crate::memories::recall::lexical_candidates;

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
        ("犬 unmatches", 1),
        ("café ai", 2),
        ("犬 café", 1),
        ("café", 1),
        ("東京都", 1),
        ("_%", 1),
        ("  ", 0),
        ("！", 0),
    ] {
        assert_eq!(
            lexical_candidates(&store, "ene", query, RecallMode::Auto)
                .unwrap()
                .len(),
            count,
            "{query:?}"
        );
    }
    assert_eq!(
        lexical_candidates(&store, "ene", "_%", RecallMode::Auto).unwrap(),
        [ai]
    );
    assert_eq!(
        lexical_candidates(&store, "ene", "東京都", RecallMode::Auto).unwrap(),
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
            lexical_candidates(&store, "ene", &query, RecallMode::Auto).unwrap(),
            rows[..20]
        );
    }
}

#[test]
fn short_recall_combines_private_and_shared_top_candidates_after_filtering() {
    let store = store();
    let repo = store.memory_repo();
    let rows = (0..90)
        .map(|i| {
            let mut record = new_memory(if i % 2 == 0 { "犬" } else { "AI" }, i % 11);
            record.scope = ["ene", SHARED_SCOPE, "other"][usize::from(i % 3)].into();
            repo.insert_memory(&record).unwrap()
        })
        .collect::<Vec<_>>();
    let excluded = HashSet::from([rows[10].id.as_str(), rows[76].id.as_str()]);

    for companion in ["ene", SHARED_SCOPE, "missing"] {
        let mut expected = rows
            .iter()
            .rev()
            .filter(|row| {
                ene_mind::in_scope(&row.scope, companion)
                    && row.importance >= 2
                    && !excluded.contains(row.id.as_str())
            })
            .collect::<Vec<_>>();
        expected.sort_by_key(|row| std::cmp::Reverse(row.importance));
        expected.truncate(20);
        assert_eq!(
            lexical_candidates(
                &store,
                companion,
                "犬 ai",
                RecallMode::AutoExcept(&excluded)
            )
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
            expected
        );
    }
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
    assert!(matches!(
        repo.control_memory(&other.id, MemoryActionKind::Share, "ene"),
        Err(StoreError::Permission(_))
    ));
    assert_eq!(repo.memory(&other.id).unwrap(), Some(other.clone()));
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
