use super::{
    Batch, COLUMNS, Commit, Fact, Memory, MemoryContext, MemoryRecord, MindUpdate, NewMemory,
    PendingEmbedding, Result, SHARED_SCOPE, Store, VISIBLE, add_edge, check_content,
    check_importance, insert_row, json, now, open_connection, read_memory_tx, row_to_memory,
    supersede_tx, to_json, truncate, validate_citations,
};
use crate::host::db::configure_connection;
use crate::host::error::HostError;
use ene_mind::{RecallMode, dimensions_of_space, in_scope};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json as value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

impl Store {
    /// The one atomic write of a `mind_update`: memories, edges, the mind
    /// state and the idempotency receipt commit together, or none do.
    pub(crate) fn apply_mind_update(&self, update: &MindUpdate) -> Result<Value> {
        if update.operation_id.is_empty() || update.operation_id.chars().count() > 128 {
            return Err(HostError::Invalid(
                ("operation_id must be 1-128 characters").into(),
            ));
        }
        let stamp = now();
        let mut connection = self.conn()?;
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
            return Err(HostError::Conflict(
                ("operation_id was already used with different content").into(),
            ));
        }
        let mut inserted = Vec::new();
        let mut updated = Vec::new();
        let mut forgotten = Vec::new();
        for memory in &update.memories {
            if memory.forget {
                let Some(id) = &memory.id else {
                    return Err(HostError::Invalid(
                        ("forget needs the id of the memory to lower").into(),
                    ));
                };
                forget_memory_tx(&transaction, update, id, memory.expected_revision)?;
                forgotten.push(id.clone());
            } else if let Some(id) = &memory.id {
                if memory.content.trim().is_empty()
                    && memory.importance.is_none()
                    && memory.supersedes.is_none()
                {
                    return Err(HostError::Invalid(
                        ("a correction needs content, importance or supersedes").into(),
                    ));
                }
                correct_memory_tx(&transaction, update, memory, id, &stamp)?;
                updated.push(id.clone());
            } else {
                if memory.content.trim().is_empty() {
                    return Err(HostError::Invalid(("memory content is required").into()));
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
                return Err(HostError::Conflict(
                    ("mind changed since it was read").into(),
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

    // === Embeddings =======================================================

    /// Opens a second read-write connection to the same database file, with
    /// the same WAL pragmas, for a background writer. The ECS thread's
    /// `Mutex<Connection>` is not held while the background task uses it
    /// (docs/design/mind.md §3.2).
    pub(crate) fn open_secondary(&self) -> Result<rusqlite::Connection> {
        let primary = self.conn()?;
        let path = primary
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| {
                HostError::Storage(
                    ("the state database has no file a second connection could open").into(),
                )
            })?;
        let connection = open_connection(Path::new(path))?;
        configure_connection(&connection)?;
        Ok(connection)
    }

    /// The `(id, memory_revision)` signature of one companion's embedding
    /// space, used to decide whether the flat index needs a rebuild. Only rows
    /// whose embedding still matches their content revision are listed, and
    /// only the ones that companion recalls: the shared pool belongs to every
    /// companion's space, another companion's scope does not.
    pub(crate) fn embedding_revisions(
        &self,
        space: &str,
        companion: &str,
    ) -> Result<Vec<(String, u64)>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(Vec::new());
        };
        let connection = self.conn()?;
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

    /// The stored BLOB and dimension count of one valid vector.
    pub(crate) fn embedding_blob(&self, id: &str, space: &str) -> Result<Option<(usize, Vec<u8>)>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(None);
        };
        let row: Option<(i64, Vec<u8>)> = self
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

    /// Memories whose vector is absent or stale for `space` and visible to
    /// `companion`, oldest first.
    pub(crate) fn memories_missing_embedding(
        &self,
        companion: &str,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        let connection = self.conn()?;
        missing_embeddings(&connection, companion, space, limit)
    }

    /// The candidate memories shown to the consolidation model: the memories
    /// the companion recalls that the job's turns actually retrieve, by FTS5
    /// and by vector similarity (`semantic` ids from `CompactVectorIndex`),
    /// then pinned and important memories only to fill the remaining slots.
    /// Forgotten memories are included when their topic matches, so the model
    /// can see that the user asked to forget them instead of re-creating them.
    pub(crate) fn consolidation_context(
        &self,
        companion: &str,
        query: &str,
        semantic: &[String],
        limit: usize,
    ) -> Result<Vec<MemoryContext>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let mut contexts = Vec::new();
        let mut push = |memory: &Memory, contexts: &mut Vec<MemoryContext>| {
            if !memory.visible_to(companion) || !seen.insert(memory.id.clone()) {
                return;
            }
            contexts.push(MemoryContext {
                id: memory.id.clone(),
                revision: memory.memory_revision,
                importance: memory.importance,
                content: memory.content.clone(),
            });
        };
        // Retrieval includes forgotten memories (Explicit): a topic the user
        // asked to forget must be visible to the extraction model so it can
        // refrain from reviving it.
        let retrieved = self.recall_hybrid(
            companion,
            &truncate(query, 2_000),
            RecallMode::Explicit,
            semantic,
            limit.saturating_mul(2),
        )?;
        for scored in &retrieved.items {
            push(&scored.memory, &mut contexts);
            if contexts.len() >= limit {
                return Ok(contexts);
            }
        }
        // Nothing (or too little) retrieved: important and recent memories are
        // still the material a merge can cite.
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m \
             WHERE {VISIBLE} AND m.importance >= 2 \
             ORDER BY m.importance DESC, COALESCE(m.last_accessed_at, m.recorded_at) DESC, \
             m.rowid DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![companion, limit as i64], row_to_memory)?;
        for memory in rows {
            push(&memory?, &mut contexts);
            if contexts.len() >= limit {
                break;
            }
        }
        Ok(contexts)
    }

    /// Commits one consolidation pass. The memory revisions captured before the
    /// model ran are checked inside the transaction; any mismatch rolls back
    /// every memory and edge the pass would have written.
    pub(crate) fn consolidate(&self, batch: &Batch, facts: &[Fact]) -> Result<Commit> {
        if batch.companion_id.trim().is_empty() {
            return Err(HostError::Invalid(
                ("consolidation batch is incomplete").into(),
            ));
        }
        let expected: HashMap<&str, u64> = batch
            .context
            .iter()
            .map(|memory| (memory.id.as_str(), memory.revision))
            .collect();
        let accepted = collect_accepted_citations(batch, facts);
        let stamp = now();
        let mut commit = Commit {
            inserted: 0,
            updated: 0,
        };
        let mut connection = self.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for fact in facts {
            apply_consolidated_fact(&transaction, batch, fact, &expected, &stamp, &mut commit)?;
        }
        record_consolidation_citations(&transaction, &accepted, batch.through, &stamp)?;
        transaction.commit()?;
        Ok(commit)
    }

    pub(crate) fn memory_edges(&self, id: &str) -> Result<Vec<(String, String)>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(
            "SELECT relation, to_id FROM memory_edges WHERE from_id = ?1 \
             UNION ALL SELECT relation, from_id FROM memory_edges WHERE to_id = ?1",
        )?;
        let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

fn collect_accepted_citations(batch: &Batch, facts: &[Fact]) -> Vec<String> {
    let mut cited = Vec::new();
    let mut unique = HashSet::new();
    for fact in facts {
        for id in &fact.cited_memory_ids {
            if unique.insert(id.as_str()) {
                cited.push(id.clone());
            }
        }
    }
    validate_citations(&cited, &batch.injected)
}

fn apply_consolidated_fact(
    transaction: &rusqlite::Transaction<'_>,
    batch: &Batch,
    fact: &Fact,
    expected: &HashMap<&str, u64>,
    stamp: &str,
    commit: &mut Commit,
) -> Result<()> {
    let content = fact.content.trim();
    if content.is_empty() {
        return Ok(());
    }
    if content.chars().count() > 4_000 {
        return Err(HostError::Invalid(
            ("consolidated memory content is too long").into(),
        ));
    }
    let importance = fact.importance.unwrap_or(3).min(10);
    // Resurrection guard (docs/design/mind.md §5.2): a superseded memory the
    // user asked to forget (`importance <= 1`) must not be brought back at a
    // higher priority by a late consolidation pass.
    if let Some(old) = fact.supersedes.as_deref()
        && let Some(current) = read_memory_tx(transaction, old)?
        && in_scope(&current.scope, &batch.companion_id)
        && current.importance <= 1
        && importance > 1
    {
        tracing::debug!(
            companion = %batch.companion_id,
            memory = %old,
            "consolidation did not revive a forgotten memory"
        );
        return Ok(());
    }
    let merge = fact
        .supersedes
        .as_deref()
        .and_then(|id| expected.get(id).copied().map(|rev| (id, rev)));
    if let Some((id, revision)) = merge {
        let current = read_memory_tx(transaction, id)?
            .ok_or_else(|| HostError::Conflict(("the superseded memory is gone").into()))?;
        if !in_scope(&current.scope, &batch.companion_id) {
            return Err(HostError::Permission(
                ("consolidation cannot merge a memory outside its scope").into(),
            ));
        }
        if current.memory_revision != revision {
            return Err(HostError::Conflict(
                ("memory changed while consolidation was running").into(),
            ));
        }
        let rows = transaction
            .prepare_cached(
                "UPDATE memories SET content = ?3, importance = ?4, \
                 content_revision = content_revision + 1, \
                 memory_revision = memory_revision + 1, last_accessed_at = ?5, \
                 embedding = NULL, embedding_model = NULL, \
                 embedding_dimensions = NULL, embedding_content_revision = NULL \
                 WHERE id = ?1 AND memory_revision = ?2",
            )?
            .execute(params![id, revision, content, importance, stamp])?;
        if rows == 0 {
            return Err(HostError::Conflict(
                ("memory changed while consolidation was running").into(),
            ));
        }
        commit.updated += 1;
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    // A fact lands in the companion's own scope unless the model asked for the
    // shared pool: one scope column holds one value.
    let scope = if fact.scope.as_deref() == Some(SHARED_SCOPE) {
        SHARED_SCOPE.to_owned()
    } else {
        batch.companion_id.clone()
    };
    insert_row(
        transaction,
        &MemoryRecord {
            id: id.clone(),
            scope,
            content: content.to_owned(),
            importance,
            occurred_at: None,
            source_context: value!({
                "source": "consolidation",
                "turn_id": batch.through.to_string(),
            }),
        },
        stamp,
    )?;
    add_edge(transaction, &batch.companion_id, "remembers", &id, stamp)?;
    if let Some(old) = fact
        .supersedes
        .as_deref()
        .filter(|old| expected.contains_key(old))
    {
        supersede_tx(transaction, &batch.companion_id, &id, old, stamp)?;
    }
    commit.inserted += 1;
    Ok(())
}

fn record_consolidation_citations(
    transaction: &rusqlite::Transaction<'_>,
    accepted: &[String],
    through: u64,
    stamp: &str,
) -> Result<()> {
    for id in accepted {
        let rows = transaction
            .prepare_cached("UPDATE memories SET last_accessed_at = ?2 WHERE id = ?1")?
            .execute(params![id, stamp])?;
        if rows > 0 {
            add_edge(
                transaction,
                id,
                "estimated_cited_in",
                &through.to_string(),
                stamp,
            )?;
        }
    }
    Ok(())
}

/// Memories whose vector is absent or stale for `space` and visible to
/// `companion`, oldest first, on the caller's connection. The embedding
/// backfill calls this with its own connection so the ECS thread's store is
/// not locked while it runs, and scoping by companion prevents one companion's
/// backfill from overwriting another companion's private memories.
pub(crate) fn missing_embeddings(
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

/// Writes a generated vector on the caller's connection, only while the
/// content that was embedded is still current. `false` means the memory
/// changed meanwhile and the vector was dropped instead of mixing spaces.
pub(crate) fn write_embedding(
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

impl Store {
    pub(crate) fn missing_embeddings_on(
        connection: &rusqlite::Connection,
        companion: &str,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        missing_embeddings(connection, companion, space, limit)
    }

    pub(crate) fn write_embedding_on(
        connection: &rusqlite::Connection,
        id: &str,
        content_revision: u64,
        space: &str,
        dimensions: usize,
        bytes: &[u8],
    ) -> Result<bool> {
        write_embedding(connection, id, content_revision, space, dimensions, bytes)
    }
}

fn forget_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    id: &str,
    expected_revision: Option<u64>,
) -> Result<()> {
    let current = read_memory_tx(transaction, id)?
        .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
    if !in_scope(&current.scope, &update.companion_id) {
        return Err(HostError::Permission(
            ("this memory is outside the companion's scope").into(),
        ));
    }
    let expected = expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(HostError::Conflict(
            ("memory changed since it was read").into(),
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
        return Err(HostError::Conflict(
            ("memory changed since it was read").into(),
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
        .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
    if !in_scope(&current.scope, &update.companion_id) {
        return Err(HostError::Permission(
            ("this memory is outside the companion's scope").into(),
        ));
    }
    let expected = memory.expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(HostError::Conflict(
            ("memory changed since it was read").into(),
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
            return Err(HostError::Conflict(
                ("memory changed since it was read").into(),
            ));
        }
    }
    if let Some(old) = &memory.supersedes {
        supersede_tx(transaction, &update.companion_id, id, old, stamp)?;
    }
    Ok(())
}

/// The profile lines `recall.rs` injects into the dynamic tail. This alpha has
/// one user profile, so the most recently updated row is the one shown.
pub(crate) fn profile_lines(store: &Store) -> Result<Vec<String>> {
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
    // A corrupt profile is dropped rather than half-injected.
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
pub(crate) fn detail(memory: &Memory, relations: &[(String, String)]) -> Value {
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
