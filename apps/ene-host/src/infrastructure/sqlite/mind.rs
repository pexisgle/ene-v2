//! The canonical mind and memory store. Every change of memories, current
//! state and the operation record is one SQLite transaction.

use super::{Error, Result};
use crate::domain::mind::*;
use serde_json::json;
use std::collections::HashMap;
use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, params};

fn profile_json(profile: &ProfileDocument) -> Result<String> {
    Ok(serde_json::to_string(profile)?)
}

fn state_json(state: &StoredState) -> Result<String> {
    Ok(serde_json::to_string(state)?)
}

/// Creates the default profile and state of a companion once.
fn ensure(tx: &Transaction<'_>, companion: &str, now: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO mind_companions \
         (companion_id,profile_revision,profile,state_revision,state,updated_at) \
         VALUES (?1,1,?2,1,?3,?4)",
        params![
            companion,
            profile_json(&ProfileDocument::default())?,
            state_json(&StoredState::default())?,
            now
        ],
    )?;
    Ok(())
}

fn read_row(
    tx: &Transaction<'_>,
    companion: &str,
) -> Result<(u64, ProfileDocument, u64, StoredState, String)> {
    let (profile_revision, profile, state_revision, state, updated_at) = tx.query_row(
        "SELECT profile_revision,profile,state_revision,state,updated_at \
         FROM mind_companions WHERE companion_id=?1",
        params![companion],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )?;
    Ok((
        profile_revision,
        serde_json::from_str(&profile).map_err(Error::Json)?,
        state_revision,
        serde_json::from_str(&state).map_err(Error::Json)?,
        updated_at,
    ))
}

pub(super) fn view(
    db: &mut tokio_rusqlite::rusqlite::Connection,
    companion: &str,
    now: &str,
) -> Result<StoredMind> {
    let tx = db.transaction()?;
    ensure(&tx, companion, now)?;
    let (profile_revision, profile, state_revision, state, updated_at) = read_row(&tx, companion)?;
    tx.commit()?;
    Ok(StoredMind {
        profile_revision,
        profile,
        state_revision,
        state,
        updated_at,
    })
}

fn memory_from_row(row: &tokio_rusqlite::rusqlite::Row<'_>) -> Result<MemoryRecord> {
    let sources: String = row.get(7)?;
    let supersedes: String = row.get(11)?;
    Ok(MemoryRecord {
        id: row.get(0)?,
        owner: row.get(1)?,
        revision: row.get::<_, i64>(2)? as u64,
        content: row.get(3)?,
        description: row.get(4)?,
        appraisal: row.get(5)?,
        acquisition: serde_json::from_value(json!(row.get::<_, String>(6)?))
            .map_err(|_| Error::CorruptReceipt)?,
        sources: serde_json::from_str(&sources).map_err(Error::Json)?,
        recorded_at: row.get(8)?,
        occurred_at: row.get(9)?,
        related: serde_json::from_str(&row.get::<_, String>(10)?).map_err(Error::Json)?,
        supersedes: serde_json::from_str(&supersedes).map_err(Error::Json)?,
        superseded_by: Vec::new(),
        recall_preference: row.get(12)?,
    })
}

fn owner_has(tx: &Transaction<'_>, owner: &str, id: &str) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM mind_memories WHERE owner=?1 AND id=?2",
            params![owner, id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn check_references(tx: &Transaction<'_>, owner: &str, request: &MindUpdateRequest) -> Result<()> {
    for id in request.referenced_memory_ids() {
        if !owner_has(tx, owner, id)? {
            return Err(Error::UnknownReference);
        }
    }
    Ok(())
}

/// Applies one idempotent mind update. A stale revision, a reference to
/// another companion's private memory, or a storage failure leaves no partial
/// change.
pub(super) fn update(
    db: &mut tokio_rusqlite::rusqlite::Connection,
    owner: &str,
    operation_id: &str,
    request_hash: &str,
    request: &MindUpdateRequest,
    source: &SourceRef,
    now: &str,
) -> Result<MindUpdateResult> {
    let tx = db.transaction()?;
    ensure(&tx, owner, now)?;
    if let Some((stored_hash, response)) = tx
        .query_row(
            "SELECT request_hash,response FROM mind_operations WHERE owner=?1 AND operation_id=?2",
            params![owner, operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if stored_hash != request_hash {
            return Err(Error::IdempotencyConflict);
        }
        let result = serde_json::from_str(&response).map_err(Error::Json)?;
        tx.commit()?;
        return Ok(result);
    }
    let (_, _, revision, mut state, _) = read_row(&tx, owner)?;
    if request.expected_revision != revision {
        return Err(Error::RevisionConflict);
    }
    check_references(&tx, owner, request)?;
    let state_changed = request
        .state
        .as_ref()
        .is_some_and(|state| !state.is_empty());
    if request.memories.is_empty() && !state_changed {
        let result = MindUpdateResult {
            operation_id: operation_id.into(),
            revision,
            changed: false,
            memories: Vec::new(),
        };
        record(&tx, owner, operation_id, request_hash, &result, now)?;
        tx.commit()?;
        return Ok(result);
    }

    let mut changed = Vec::new();
    for memory in &request.memories {
        let id = uuid::Uuid::new_v4().to_string();
        let mut sources = vec![source.clone()];
        sources.extend(memory.sources.iter().cloned());
        tx.execute(
            "INSERT INTO mind_memories \
             (id,owner,revision,content,description,appraisal,acquisition,sources,recorded_at,occurred_at,related,supersedes,recall_preference,updated_at) \
             VALUES (?1,?2,1,?3,?4,?5,?6,?7,?8,?9,?10,?11,'normal',?8)",
            params![
                id,
                owner,
                memory.content,
                memory.description,
                memory.appraisal,
                acquisition_str(memory.acquisition),
                serde_json::to_string(&sources)?,
                now,
                memory.occurred_at,
                "[]",
                serde_json::to_string(&memory.supersedes)?,
            ],
        )?;
        changed.push(ChangedMemory {
            id,
            supersedes: memory.supersedes.clone(),
        });
    }
    if let Some(update) = &request.state {
        if let Some(emotion) = &update.emotion {
            state.emotion = Some(serde_json::to_value(emotion).map_err(Error::Json)?);
        }
        if let Some(relationships) = &update.relationships {
            state.relationships = relationships
                .iter()
                .map(|relationship| serde_json::to_value(relationship).map_err(Error::Json))
                .collect::<Result<Vec<_>>>()?;
        }
        if let Some(tendencies) = &update.tendencies {
            state.tendencies = tendencies.clone();
        }
        if let Some(concerns) = &update.concerns {
            state.concerns = concerns.clone();
        }
    }
    let revision = revision
        .checked_add(1)
        .ok_or_else(|| Error::StateConflict)?;
    tx.execute(
        "UPDATE mind_companions SET state_revision=?2,state=?3,updated_at=?4 WHERE companion_id=?1",
        params![owner, revision as i64, state_json(&state)?, now],
    )?;
    let result = MindUpdateResult {
        operation_id: operation_id.into(),
        revision,
        changed: true,
        memories: changed,
    };
    record(&tx, owner, operation_id, request_hash, &result, now)?;
    tx.commit()?;
    Ok(result)
}

fn record(
    tx: &Transaction<'_>,
    owner: &str,
    operation_id: &str,
    request_hash: &str,
    result: &MindUpdateResult,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO mind_operations \
         (owner,operation_id,request_hash,response,revision,updated_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            owner,
            operation_id,
            request_hash,
            serde_json::to_string(result)?,
            result.revision as i64,
            now
        ],
    )?;
    Ok(())
}

fn acquisition_str(acquisition: Acquisition) -> &'static str {
    match acquisition {
        Acquisition::DirectExperience => "direct_experience",
        Acquisition::UserStatement => "user_statement",
        Acquisition::Report => "report",
        Acquisition::Shared => "shared",
        Acquisition::Inference => "inference",
    }
}

fn like(query: &str) -> String {
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// The full phrase plus Japanese character bigrams. This is the step 2 lexical
/// search: it finds a stored memory for a natural short phrase without a
/// tokenizer or embeddings. The trigram/semantic search of step 3 replaces it
/// behind the same recall contract.
fn search_terms(query: &str) -> Vec<String> {
    let trimmed = query.trim();
    let mut terms = vec![trimmed.to_owned()];
    let chars = trimmed.chars().collect::<Vec<_>>();
    for window in chars.windows(2) {
        if terms.len() >= 16 {
            break;
        }
        let term = window.iter().collect::<String>();
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn superseded_by(
    db: &tokio_rusqlite::rusqlite::Connection,
    owner: &str,
) -> Result<HashMap<String, Vec<String>>> {
    let mut statement = db.prepare("SELECT id,supersedes FROM mind_memories WHERE owner=?1")?;
    let rows = statement.query_map(params![owner], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (id, supersedes) = row?;
        let supersedes: Vec<String> = serde_json::from_str(&supersedes).map_err(Error::Json)?;
        for target in supersedes {
            map.entry(target).or_default().push(id.clone());
        }
    }
    Ok(map)
}

pub(super) fn recall(
    db: &mut tokio_rusqlite::rusqlite::Connection,
    owner: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<MemoryRecord>> {
    let terms = search_terms(query);
    let mut sql = String::from(
        "SELECT id,owner,revision,content,description,appraisal,acquisition,sources,recorded_at,occurred_at,related,supersedes,recall_preference \
         FROM mind_memories WHERE owner=?1 AND (",
    );
    let mut values: Vec<tokio_rusqlite::rusqlite::types::Value> = vec![owner.to_owned().into()];
    for (index, term) in terms.iter().enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        let placeholder = index + 2;
        sql.push_str(&format!(
            "(content LIKE ?{placeholder} ESCAPE '\\' OR description LIKE ?{placeholder} ESCAPE '\\' OR appraisal LIKE ?{placeholder} ESCAPE '\\')"
        ));
        values.push(like(term).into());
    }
    sql.push_str(&format!(
        ") ORDER BY recorded_at DESC,id LIMIT ?{}",
        values.len() + 1
    ));
    values.push((limit as i64).into());
    let mut statement = db.prepare(&sql)?;
    let mut memories = statement
        .query_map(tokio_rusqlite::rusqlite::params_from_iter(values), |row| {
            memory_from_row(row).map_err(|error| match error {
                Error::Sql(error) => error,
                _ => tokio_rusqlite::rusqlite::Error::InvalidQuery,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(statement);
    let superseded = superseded_by(db, owner)?;
    for memory in &mut memories {
        memory.superseded_by = superseded.get(&memory.id).cloned().unwrap_or_default();
    }
    Ok(memories)
}

pub(super) fn read(
    db: &mut tokio_rusqlite::rusqlite::Connection,
    owner: &str,
    id: &str,
) -> Result<Option<MemoryRecord>> {
    let memory = db
        .query_row(
            "SELECT id,owner,revision,content,description,appraisal,acquisition,sources,recorded_at,occurred_at,related,supersedes,recall_preference \
             FROM mind_memories WHERE owner=?1 AND id=?2",
            params![owner, id],
            |row| {
                memory_from_row(row).map_err(|error| match error {
                    Error::Sql(error) => error,
                    _ => tokio_rusqlite::rusqlite::Error::InvalidQuery,
                })
            },
        )
        .optional()?;
    let Some(mut memory) = memory else {
        return Ok(None);
    };
    memory.superseded_by = superseded_by(db, owner)?
        .get(id)
        .cloned()
        .unwrap_or_default();
    Ok(Some(memory))
}

pub(super) fn update_profile(
    db: &mut tokio_rusqlite::rusqlite::Connection,
    companion: &str,
    expected_revision: u64,
    profile: &ProfileDocument,
    now: &str,
) -> Result<(u64, ProfileDocument)> {
    let tx = db.transaction()?;
    ensure(&tx, companion, now)?;
    let (revision, current, ..) = read_row(&tx, companion)?;
    if revision != expected_revision {
        return Err(Error::RevisionConflict);
    }
    if current == *profile {
        tx.commit()?;
        return Ok((revision, current));
    }
    let revision = revision.checked_add(1).ok_or(Error::StateConflict)?;
    tx.execute(
        "UPDATE mind_companions SET profile_revision=?2,profile=?3,updated_at=?4 WHERE companion_id=?1",
        params![companion, revision as i64, profile_json(profile)?, now],
    )?;
    tx.commit()?;
    Ok((revision, profile.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mind::{Acquisition, Emotion, MindUpdateRequest, NewMemory, SourceKind};
    use crate::infrastructure::sqlite::Store;

    fn source() -> SourceRef {
        SourceRef {
            kind: SourceKind::Input,
            id: "request-1".into(),
            entry_id: Some("msg-1".into()),
            speaker: Some("user".into()),
            observed_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    fn memory(content: &str) -> NewMemory {
        NewMemory {
            content: content.into(),
            description: String::new(),
            appraisal: String::new(),
            acquisition: Acquisition::UserStatement,
            supersedes: Vec::new(),
            sources: Vec::new(),
            occurred_at: None,
        }
    }

    fn request(expected: u64, memories: Vec<NewMemory>) -> MindUpdateRequest {
        MindUpdateRequest {
            expected_revision: expected,
            memories,
            state: None,
        }
    }

    fn state_change() -> crate::domain::mind::StateUpdate {
        crate::domain::mind::StateUpdate {
            emotion: Some(Emotion {
                description: "少しうれしい".into(),
                target: Some("ユーザー".into()),
                reason: Some("好みを話してくれた".into()),
                signals: Vec::new(),
            }),
            ..Default::default()
        }
    }

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("mind.db")).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn retries_are_idempotent_and_do_not_duplicate() {
        let (_dir, store) = store().await;
        let (first, second) = store
            .run(|db| {
                let one = update(
                    db,
                    "a",
                    "op-1",
                    "hash-1",
                    &request(1, vec![memory("柚子が好き")]),
                    &source(),
                    "2026-01-01T00:00:00Z",
                )?;
                let two = update(
                    db,
                    "a",
                    "op-1",
                    "hash-1",
                    &request(1, vec![memory("柚子が好き")]),
                    &source(),
                    "2026-01-01T00:00:01Z",
                )?;
                Ok((one, two))
            })
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.revision, 2);
        assert!(first.changed);
        store
            .run(|db| {
                let count: i64 = db.query_row(
                    "SELECT COUNT(*) FROM mind_memories WHERE owner='a'",
                    [],
                    |r| r.get(0),
                )?;
                assert_eq!(count, 1);
                let different = update(
                    db,
                    "a",
                    "op-1",
                    "hash-2",
                    &request(1, Vec::new()),
                    &source(),
                    "2026-01-01T00:00:02Z",
                );
                assert!(matches!(different, Err(Error::IdempotencyConflict)));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stale_revisions_and_foreign_references_change_nothing() {
        let (_dir, store) = store().await;
        store
            .run(|db| {
                assert!(matches!(
                    update(
                        db,
                        "a",
                        "op-1",
                        "h",
                        &request(9, vec![memory("x")]),
                        &source(),
                        "now",
                    ),
                    Err(Error::RevisionConflict)
                ));
                let mut foreign = memory("y");
                foreign.supersedes = vec!["missing".into()];
                assert!(matches!(
                    update(
                        db,
                        "a",
                        "op-2",
                        "h2",
                        &request(1, vec![foreign]),
                        &source(),
                        "now",
                    ),
                    Err(Error::UnknownReference)
                ));
                // A memory owned by a is not visible to b.
                update(
                    db,
                    "a",
                    "op-3",
                    "h3",
                    &request(1, vec![memory("a private")]),
                    &source(),
                    "now",
                )?;
                let id: String =
                    db.query_row("SELECT id FROM mind_memories WHERE owner='a'", [], |r| {
                        r.get(0)
                    })?;
                let mut stolen = memory("b tries");
                stolen.supersedes = vec![id];
                assert!(matches!(
                    update(
                        db,
                        "b",
                        "op-4",
                        "h4",
                        &request(1, vec![stolen]),
                        &source(),
                        "now",
                    ),
                    Err(Error::UnknownReference)
                ));
                Ok(())
            })
            .await
            .unwrap();
        store
            .run(|db| {
                let views = view(db, "a", "now")?;
                assert_eq!(views.state_revision, 2);
                assert_eq!(
                    db.query_row(
                        "SELECT COUNT(*) FROM mind_memories WHERE owner='b'",
                        [],
                        |r| { r.get::<_, i64>(0) }
                    )?,
                    0
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_change_is_a_normal_result_and_is_replayed() {
        let (_dir, store) = store().await;
        let (first, second) = store
            .run(|db| {
                let first = update(
                    db,
                    "a",
                    "op-1",
                    "h",
                    &request(1, Vec::new()),
                    &source(),
                    "now",
                )?;
                let second = update(
                    db,
                    "a",
                    "op-1",
                    "h",
                    &request(1, Vec::new()),
                    &source(),
                    "later",
                )?;
                Ok((first, second))
            })
            .await
            .unwrap();
        assert!(!first.changed);
        assert_eq!(first.revision, 1);
        assert_eq!(first, second);
        store
            .run(|db| {
                assert_eq!(view(db, "a", "now")?.state_revision, 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_failed_memory_insert_rolls_back_state_and_operation() {
        let (_dir, store) = store().await;
        store
            .run(|db| {
                db.execute_batch(
                    "CREATE TRIGGER fail_memory BEFORE INSERT ON mind_memories \
                     BEGIN SELECT RAISE(ABORT, 'no'); END;",
                )?;
                let mut request = request(1, vec![memory("rollback")]);
                request.state = Some(state_change());
                assert!(update(db, "a", "op-1", "h", &request, &source(), "now").is_err());
                db.execute_batch("DROP TRIGGER fail_memory;")?;
                let stored = view(db, "a", "now")?;
                assert_eq!(stored.state_revision, 1);
                assert!(stored.state.emotion.is_none());
                let operations: i64 =
                    db.query_row("SELECT COUNT(*) FROM mind_operations", [], |r| r.get(0))?;
                assert_eq!(operations, 0);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn correction_keeps_the_old_memory_with_its_link() {
        let (_dir, store) = store().await;
        let (old_id, new_id) = store
            .run(|db| {
                let old = update(
                    db,
                    "a",
                    "op-1",
                    "h1",
                    &request(1, vec![memory("柚子が好き")]),
                    &source(),
                    "2026-01-01T00:00:00Z",
                )?;
                let old_id = old.memories[0].id.clone();
                let mut correction = memory("実はりんごが好き");
                correction.supersedes = vec![old_id.clone()];
                correction.acquisition = Acquisition::DirectExperience;
                let new = update(
                    db,
                    "a",
                    "op-2",
                    "h2",
                    &request(2, vec![correction]),
                    &source(),
                    "2026-01-01T00:00:01Z",
                )?;
                Ok((old_id, new.memories[0].id.clone()))
            })
            .await
            .unwrap();
        store
            .run(move |db| {
                let old = read(db, "a", &old_id)?.expect("old memory");
                assert_eq!(old.superseded_by, vec![new_id.clone()]);
                let new = read(db, "a", &new_id)?.expect("new memory");
                assert_eq!(new.supersedes, vec![old_id.clone()]);
                assert_eq!(old.content, "柚子が好き");
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recall_searches_only_the_owner() {
        let (_dir, store) = store().await;
        store
            .run(|db| {
                let mut request_a =
                    request(1, vec![memory("柚子が好き"), memory("パクチーが苦手")]);
                request_a.expected_revision = 1;
                update(
                    db,
                    "a",
                    "op-1",
                    "h1",
                    &request_a,
                    &source(),
                    "2026-01-01T00:00:00Z",
                )?;
                update(
                    db,
                    "b",
                    "op-2",
                    "h2",
                    &request(1, vec![memory("柚子はbの秘密")]),
                    &source(),
                    "2026-01-01T00:00:01Z",
                )?;
                let found = recall(db, "a", "柚子", 10)?;
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].owner, "a");
                assert!(found[0].content.contains("柚子"));
                assert!(recall(db, "a", "qqqq", 10)?.is_empty());
                assert!(read(db, "a", "b-memory")?.is_none());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn profile_and_mind_survive_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mind.db");
        let store = Store::open(path.clone()).await.unwrap();
        let profile = ProfileDocument {
            facets: vec!["静かだが observant".into()],
            examples: vec!["「なるほど」と短く返す".into()],
            ..Default::default()
        };
        let profile_for_write = profile.clone();
        store
            .run(move |db| {
                update(
                    db,
                    "a",
                    "op-1",
                    "h",
                    &request(1, vec![memory("一緒に山へ行った")]),
                    &source(),
                    "now",
                )?;
                let (revision, stored) = update_profile(db, "a", 1, &profile_for_write, "now")?;
                assert_eq!(revision, 2);
                assert_eq!(stored, profile_for_write);
                Ok(())
            })
            .await
            .unwrap();
        store.db.clone().close().await.unwrap();
        drop(store);
        let reopened = Store::open(path).await.unwrap();
        reopened
            .run(move |db| {
                let stored = view(db, "a", "now")?;
                assert_eq!(stored.state_revision, 2);
                assert_eq!(stored.profile_revision, 2);
                assert_eq!(stored.profile, profile);
                let found = recall(db, "a", "山", 10)?;
                assert_eq!(found.len(), 1);
                assert_eq!(found[0].content, "一緒に山へ行った");
                Ok(())
            })
            .await
            .unwrap();
    }
}
