use super::{
    Memory, MemoryRecord, MemoryRepo, MindUpdate, NewMemory, PendingEmbedding, Result, VISIBLE,
    add_edge, check_content, check_importance, insert_row, json, now, read_memory_tx, supersede_tx,
    to_json,
};
use crate::{Store, StoreError};
use ene_mind::{dimensions_of_space, in_scope};
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
        let mut connection = self.0.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
            if memory.forget {
                let Some(id) = &memory.id else {
                    return Err(StoreError::Invalid(
                        "forget needs the id of the memory to lower".into(),
                    ));
                };
                forget_memory_tx(&transaction, update, id, memory.expected_revision)?;
                forgotten.push(id.clone());
            } else if let Some(id) = &memory.id {
                if memory.content.trim().is_empty()
                    && memory.importance.is_none()
                    && memory.supersedes.is_none()
                {
                    return Err(StoreError::Invalid(
                        "a correction needs content, importance or supersedes".into(),
                    ));
                }
                correct_memory_tx(&transaction, update, memory, id, &stamp)?;
                updated.push(id.clone());
            } else {
                if memory.content.trim().is_empty() {
                    return Err(StoreError::Invalid("memory content is required".into()));
                }
                let id = uuid::Uuid::new_v4().to_string();
                insert_row(
                    &transaction,
                    &MemoryRecord {
                        id: id.clone(),
                        scope: update.companion_id.clone(),
                        content: memory.content.clone(),
                        importance: memory.importance.unwrap_or(3).min(10),
                        occurred_at: memory.occurred_at.clone(),
                        source_context: value!({
                            "source": "mind_update",
                            "operation_id": update.operation_id,
                        }),
                    },
                    &stamp,
                )?;
                add_edge(&transaction, &update.companion_id, "remembers", &id, &stamp)?;
                if let Some(old) = &memory.supersedes {
                    supersede_tx(&transaction, &update.companion_id, &id, old, &stamp)?;
                }
                inserted.push(id);
            }
        }
        let mut state_revision = None;
        if let Some(state) = &update.state {
            let rows = transaction
                .prepare_cached(
                    "UPDATE companions SET state = ?2, state_revision = state_revision + 1, \
                     updated_at = ?3 WHERE id = ?1 AND state_revision = ?4",
                )?
                .execute(params![
                    update.companion_id,
                    state.state,
                    stamp,
                    state.expected_revision
                ])?;
            if rows == 0 {
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
        transaction.commit()?;
        Ok(result)
    }

    pub fn embedding_revisions(&self, space: &str, companion: &str) -> Result<Vec<(String, u64)>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(Vec::new());
        };
        let connection = self.0.conn()?;
        let mut statement = connection.prepare_cached(&format!(
            "SELECT m.id, m.memory_revision FROM memories m \
             WHERE {VISIBLE} AND m.embedding_model = ?2 AND m.embedding_dimensions = ?3 \
             AND m.embedding IS NOT NULL AND m.embedding_content_revision = m.content_revision \
             ORDER BY m.rowid"
        ))?;
        let rows = statement.query_map(params![companion, space, dimensions as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn embedding_blob(&self, id: &str, space: &str) -> Result<Option<(usize, Vec<u8>)>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(None);
        };
        let row: Option<(i64, Vec<u8>)> = self
            .0
            .conn()?
            .prepare_cached(
                "SELECT embedding_dimensions, embedding FROM memories WHERE id = ?1 \
                 AND embedding_model = ?2 AND embedding_dimensions = ?3 \
                 AND embedding IS NOT NULL AND embedding_content_revision = content_revision",
            )?
            .query_row(params![id, space, dimensions as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        Ok(row.map(|(dimensions, bytes)| (dimensions as usize, bytes)))
    }

    pub fn memories_missing_embedding(
        &self,
        companion: &str,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        let connection = self.0.conn()?;
        missing_embeddings_on(&connection, companion, space, limit)
    }

    pub fn memory_edges(&self, id: &str) -> Result<Vec<(String, String)>> {
        let connection = self.0.conn()?;
        let mut statement = connection.prepare_cached(
            "SELECT relation, to_id FROM memory_edges WHERE from_id = ?1 \
             UNION ALL SELECT relation, from_id FROM memory_edges WHERE to_id = ?1",
        )?;
        let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

/// Memories whose vector is absent or stale for `space` and visible to `companion`.
pub fn missing_embeddings_on(
    connection: &rusqlite::Connection,
    companion: &str,
    space: &str,
    limit: usize,
) -> Result<Vec<PendingEmbedding>> {
    let Some(dimensions) = dimensions_of_space(space) else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare_cached(&format!(
        "SELECT m.id, m.content, m.content_revision FROM memories m \
         WHERE {VISIBLE} AND (m.embedding IS NULL OR m.embedding_model IS NOT ?2 \
         OR m.embedding_dimensions IS NOT ?3 OR m.embedding_content_revision IS NULL \
         OR m.embedding_content_revision != m.content_revision) \
         ORDER BY m.rowid LIMIT ?4",
    ))?;
    let rows = statement.query_map(
        params![companion, space, dimensions as i64, limit as i64],
        |row| {
            Ok(PendingEmbedding {
                id: row.get(0)?,
                content: row.get(1)?,
                content_revision: row.get(2)?,
            })
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Writes a generated vector on `connection` while the embedded content revision matches.
pub fn write_embedding_on(
    connection: &rusqlite::Connection,
    id: &str,
    content_revision: u64,
    space: &str,
    dimensions: usize,
    bytes: &[u8],
) -> Result<bool> {
    let updated = connection
        .prepare_cached(
            "UPDATE memories SET embedding = ?3, embedding_model = ?4, \
             embedding_dimensions = ?5, embedding_content_revision = content_revision \
             WHERE id = ?1 AND content_revision = ?2",
        )?
        .execute(params![
            id,
            content_revision,
            bytes,
            space,
            dimensions as i64
        ])?;
    Ok(updated > 0)
}

fn forget_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    id: &str,
    expected_revision: Option<u64>,
) -> Result<()> {
    let current = read_memory_tx(transaction, id)?
        .ok_or_else(|| StoreError::NotFound("memory not found".into()))?;
    if !in_scope(&current.scope, &update.companion_id) {
        return Err(StoreError::Permission(
            "this memory is outside the companion's scope".into(),
        ));
    }
    let expected = expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(StoreError::Conflict(
            "memory changed since it was read".into(),
        ));
    }
    let rows = transaction
        .prepare_cached(
            "UPDATE memories SET importance = 0, \
             memory_revision = memory_revision + 1 \
             WHERE id = ?1 AND memory_revision = ?2",
        )?
        .execute(params![id, expected])?;
    if rows == 0 {
        return Err(StoreError::Conflict(
            "memory changed since it was read".into(),
        ));
    }
    Ok(())
}

fn correct_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    memory: &NewMemory,
    id: &str,
    stamp: &str,
) -> Result<()> {
    let current = read_memory_tx(transaction, id)?
        .ok_or_else(|| StoreError::NotFound("memory not found".into()))?;
    if !in_scope(&current.scope, &update.companion_id) {
        return Err(StoreError::Permission(
            "this memory is outside the companion's scope".into(),
        ));
    }
    let expected = memory.expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(StoreError::Conflict(
            "memory changed since it was read".into(),
        ));
    }
    let content = if memory.content.trim().is_empty() {
        current.content.clone()
    } else {
        memory.content.clone()
    };
    check_content(&content)?;
    let importance = memory.importance.unwrap_or(current.importance);
    check_importance(importance)?;
    let content_changed = content != current.content;
    let importance_changed = importance != current.importance;
    if content_changed || importance_changed {
        let changed_content = if content_changed { 1i64 } else { 0i64 };
        let rows = transaction
            .prepare_cached(
                "UPDATE memories SET content = ?3, importance = ?4, \
                 content_revision = content_revision + ?5, \
                 memory_revision = memory_revision + 1, last_accessed_at = ?6, \
                 embedding = CASE WHEN ?5 = 1 THEN NULL ELSE embedding END, \
                 embedding_model = CASE WHEN ?5 = 1 THEN NULL ELSE embedding_model END, \
                 embedding_dimensions = CASE WHEN ?5 = 1 THEN NULL ELSE embedding_dimensions END, \
                 embedding_content_revision = CASE WHEN ?5 = 1 THEN NULL \
                     ELSE embedding_content_revision END \
                 WHERE id = ?1 AND memory_revision = ?2",
            )?
            .execute(params![
                id,
                expected,
                content,
                importance,
                changed_content,
                stamp
            ])?;
        if rows == 0 {
            return Err(StoreError::Conflict(
                "memory changed since it was read".into(),
            ));
        }
    }
    if let Some(old) = &memory.supersedes {
        supersede_tx(transaction, &update.companion_id, id, old, stamp)?;
    }
    Ok(())
}

/// The profile lines injected into dynamic context recall.
pub fn profile_lines(store: &Store) -> Result<Vec<String>> {
    let connection = store.conn()?;
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
