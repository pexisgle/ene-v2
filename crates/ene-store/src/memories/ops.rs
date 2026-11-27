use super::{
    Memory, MemoryRecord, MemoryRepo, MindStateChange, MindUpdate, NewMemory, PendingEmbedding,
    Result, VISIBLE, add_edge, check_importance, insert_row, json, now, supersede_tx, to_json,
    visible_memory,
};
use crate::{Store, StoreError};
use ene_mind::{RecallCandidate, dimensions_of_space};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json as value};

impl<'a> MemoryRepo<'a> {
    /// The one atomic write of a `mind_update`: memories, edges, the mind
    /// state and the idempotency receipt commit together, or none do.
    pub fn apply_mind_update(&self, update: &MindUpdate) -> Result<Value> {
        if update.operation_id.is_empty() || update.operation_id.chars().count() > 128 {
            return Err(StoreError::Invalid(
                "operation_id must be 1-128 characters".into(),
            ));
        }
        let stamp = now();
        self.0.write_tx(|transaction| {
            let existing: Option<(String, Vec<u8>, String)> = transaction
                .prepare_cached(
                    "SELECT companion_id, request_hash, result FROM mind_operations \
                     WHERE operation_id = ?1",
                )?
                .query_row([&update.operation_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()?;
            if let Some((companion_id, hash, stored)) = existing {
                if companion_id == update.companion_id && hash == update.request_hash {
                    return json(&stored);
                }
                return Err(StoreError::Conflict(
                    "operation_id was already used with different content".into(),
                ));
            }
            let mut inserted = Vec::new();
            let mut updated = Vec::new();
            let mut forgotten = Vec::new();
            for memory in &update.memories {
                let Some((id, revision)) = mutation_target(memory)? else {
                    inserted.push(insert_mind_memory(
                        transaction,
                        update,
                        memory,
                        None,
                        memory.importance.unwrap_or(3).min(10),
                        &stamp,
                    )?);
                    continue;
                };
                let current = editable_memory(transaction, &update.companion_id, id, revision)?;
                if memory.forget {
                    forget_memory_tx(transaction, &current)?;
                    forgotten.push(current.id);
                } else {
                    inserted.extend(correct_memory_tx(
                        transaction,
                        update,
                        memory,
                        &current,
                        &stamp,
                    )?);
                    updated.push(current.id);
                }
            }
            let mut state_revision = None;
            if let Some(state) = &update.state {
                if !update_mind_state(transaction, &update.companion_id, state, &stamp)? {
                    return Err(StoreError::Conflict(
                        "mind changed since it was read".into(),
                    ));
                }
                state_revision = Some(state.expected_revision + 1);
            }
            let result = value!({
                "memories": inserted,
                "updated": updated,
                "forgotten": forgotten,
                "state": update.state.is_some(),
                "state_revision": state_revision,
                "operation_id": update.operation_id,
            });
            transaction
                .prepare_cached(
                    "INSERT INTO mind_operations \
                     (operation_id, companion_id, request_hash, result, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    update.operation_id,
                    update.companion_id,
                    update.request_hash,
                    to_json(&result)?,
                    stamp
                ])?;
            Ok(result)
        })
    }

    pub fn embedding_revisions(
        &self,
        space: &str,
        companion: &str,
    ) -> Result<Vec<(RecallCandidate, u8)>> {
        self.0.fetch_all(
            &format!(
                "SELECT m.id, m.content_revision, m.importance FROM memory_embeddings e \
                 JOIN memories m ON m.id = e.memory_id \
                 WHERE {VISIBLE} AND e.space = ?2 ORDER BY m.rowid"
            ),
            params![companion, space],
            |row| {
                Ok((
                    RecallCandidate {
                        id: row.get(0)?,
                        content_revision: row.get(1)?,
                    },
                    row.get(2)?,
                ))
            },
        )
    }

    pub fn embedding_blob(
        &self,
        candidate: &RecallCandidate,
        space: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.0.fetch(
            "SELECT e.vector FROM memory_embeddings e JOIN memories m ON m.id = e.memory_id \
             WHERE e.memory_id = ?1 AND e.space = ?2 AND m.content_revision = ?3",
            params![candidate.id, space, candidate.content_revision],
            |row| row.get(0),
        )
    }

    pub fn memory_edges(&self, id: &str) -> Result<Vec<(String, String)>> {
        memory_edges(&self.0.conn(), id)
    }

    /// Memories whose vector is absent or stale for `space` and visible to `companion`.
    pub fn memories_missing_embedding(
        &self,
        companion: &str,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        self.0.fetch_all(
            &format!(
                "SELECT m.id, m.content_revision, m.content FROM memories m \
                 WHERE {VISIBLE} AND NOT EXISTS (SELECT 1 FROM memory_embeddings e \
                     WHERE e.memory_id = m.id AND e.space = ?2) ORDER BY m.rowid LIMIT ?3"
            ),
            params![companion, space, limit as i64],
            |row| {
                Ok(PendingEmbedding {
                    candidate: RecallCandidate {
                        id: row.get(0)?,
                        content_revision: row.get(1)?,
                    },
                    content: row.get(2)?,
                })
            },
        )
    }

    /// Stores the first completed vector for the visible content revision and space.
    pub fn write_embedding(
        &self,
        companion: &str,
        candidate: &RecallCandidate,
        space: &str,
        bytes: &[u8],
    ) -> Result<bool> {
        let expected =
            dimensions_of_space(space).and_then(|dims| dims.checked_mul(size_of::<f32>()));
        if bytes.is_empty() || expected != Some(bytes.len()) {
            return Err(StoreError::Invalid(
                "embedding does not match its space dimensions".into(),
            ));
        }
        let inserted = self
            .0
            .conn()
            .prepare_cached(&format!(
                "INSERT INTO memory_embeddings (memory_id, space, vector) \
             SELECT m.id, ?4, ?5 FROM memories m \
             WHERE {VISIBLE} AND m.id = ?2 AND m.content_revision = ?3 \
             ON CONFLICT (memory_id, space) DO NOTHING"
            ))?
            .execute(params![
                companion,
                candidate.id,
                candidate.content_revision,
                space,
                bytes
            ])?;
        Ok(inserted != 0)
    }
}

pub(super) fn update_mind_state(
    transaction: &rusqlite::Transaction<'_>,
    companion: &str,
    state: &MindStateChange,
    stamp: &str,
) -> Result<bool> {
    state.state.validate()?;
    Ok(transaction
        .prepare_cached(
            "UPDATE companions SET state = ?2, state_revision = state_revision + 1, \
         updated_at = ?3 WHERE id = ?1 AND state_revision = ?4",
        )?
        .execute(params![
            companion,
            to_json(&state.state)?,
            stamp,
            state.expected_revision
        ])?
        != 0)
}

pub(super) fn memory_edges(
    connection: &rusqlite::Connection,
    id: &str,
) -> Result<Vec<(String, String)>> {
    let mut statement = connection.prepare_cached(
        "SELECT relation, to_id FROM memory_edges WHERE from_id = ?1 \
         UNION ALL SELECT relation, from_id FROM memory_edges WHERE to_id = ?1",
    )?;
    let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn mutation_target(memory: &NewMemory) -> Result<Option<(&str, u64)>> {
    let id = match (memory.id.as_deref(), memory.supersedes.as_deref()) {
        (Some(id), Some(other)) if id != other => {
            return Err(StoreError::Invalid("specify one memory to change".into()));
        }
        (id, supersedes) => id.or(supersedes),
    };
    match (id, memory.expected_revision) {
        (Some(id), Some(revision)) => Ok(Some((id, revision))),
        (None, None) if !memory.forget => Ok(None),
        _ => Err(StoreError::Invalid(
            "changing a memory requires its id and expected_revision".into(),
        )),
    }
}

fn forget_memory_tx(transaction: &rusqlite::Transaction<'_>, current: &Memory) -> Result<()> {
    transaction
        .prepare_cached(
            "UPDATE memories SET importance = 0, \
             memory_revision = memory_revision + 1 \
             WHERE id = ?1",
        )?
        .execute([&current.id])?;
    Ok(())
}

fn correct_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    memory: &NewMemory,
    current: &Memory,
    stamp: &str,
) -> Result<Option<String>> {
    let importance = memory.importance.unwrap_or(current.importance);
    check_importance(importance)?;
    if memory.content.trim().is_empty() && memory.importance.is_none() {
        return Err(StoreError::Invalid(
            "a correction needs content or importance".into(),
        ));
    }
    if !memory.content.trim().is_empty() && memory.content != current.content {
        let next = insert_mind_memory(
            transaction,
            update,
            memory,
            Some(current),
            importance,
            stamp,
        )?;
        supersede_tx(transaction, &update.companion_id, &next, &current.id, stamp)?;
        return Ok(Some(next));
    }
    if importance != current.importance {
        transaction
            .prepare_cached(
                "UPDATE memories SET importance = ?2, \
                 memory_revision = memory_revision + 1, last_accessed_at = ?3 WHERE id = ?1",
            )?
            .execute(params![current.id, importance, stamp])?;
    }
    Ok(None)
}

fn editable_memory(
    connection: &rusqlite::Connection,
    companion: &str,
    id: &str,
    expected: u64,
) -> Result<Memory> {
    let current = visible_memory(connection, id, companion)?;
    if expected != current.memory_revision {
        return Err(StoreError::Conflict(
            "memory changed since it was read".into(),
        ));
    }
    Ok(current)
}

fn insert_mind_memory(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    memory: &NewMemory,
    prior: Option<&Memory>,
    importance: u8,
    stamp: &str,
) -> Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    insert_row(
        transaction,
        &MemoryRecord {
            id: id.clone(),
            scope: prior.map_or_else(|| update.companion_id.clone(), |old| old.scope.clone()),
            content: memory.content.clone(),
            importance,
            occurred_at: memory
                .occurred_at
                .clone()
                .or_else(|| prior?.occurred_at.clone()),
            source_context: value!({
                "source": if prior.is_some() { "mind_correction" } else { "mind_update" },
                "operation_id": update.operation_id,
            }),
        },
        stamp,
    )?;
    add_edge(transaction, &update.companion_id, "remembers", &id, stamp)?;
    Ok(id)
}

/// The profile lines injected into dynamic context recall.
pub fn profile_lines(store: &Store) -> Result<Vec<String>> {
    let connection = store.conn();
    let mut statement = connection.prepare_cached(
        "SELECT name, preferences, static_constraints FROM user_profiles \
         ORDER BY updated_at DESC, id LIMIT 1",
    )?;
    let row: Option<(String, String, String)> = statement
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .optional()?;
    let Some((name, preferences, static_constraints)) = row else {
        return Ok(Vec::new());
    };
    let (Ok(preferences), Ok(static_constraints)) = (
        serde_json::from_str::<Vec<String>>(&preferences),
        serde_json::from_str::<Vec<String>>(&static_constraints),
    ) else {
        return Ok(Vec::new());
    };
    let mut lines = Vec::new();
    if !name.trim().is_empty() {
        lines.push(format!("名前: {}", name.trim()));
    }
    if !preferences.is_empty() {
        lines.push(format!("好み: {}", preferences.join("、")));
    }
    if !static_constraints.is_empty() {
        lines.push(format!("制約: {}", static_constraints.join("、")));
    }
    Ok(lines)
}

/// The memory detail returned by `mind_read`: the record plus its edges.
pub fn detail(memory: &Memory, relations: &[(String, String)]) -> Value {
    value!({
        "id": memory.id,
        "scope": memory.scope,
        "content": memory.content,
        "importance": memory.importance,
        "occurred_at": memory.occurred_at,
        "recorded_at": memory.recorded_at,
        "last_accessed_at": memory.last_accessed_at,
        "revision": memory.memory_revision,
        "emotion": memory.emotion,
        "source": memory.source_context,
        "relations": relations.iter().map(|(relation, to)| value!({
            "relation": relation, "to": to
        })).collect::<Vec<_>>(),
    })
}
