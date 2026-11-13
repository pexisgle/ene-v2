//! Memory persistence: insertion, CAS updates, sharing, hybrid recall,
//! consolidation commits and the user profile.
use super::{Result, Store, digest, json, now, to_json};
use crate::host::error::HostError;
use crate::host::mind::consolidate::{
    ConsolidationBatch, ConsolidationCommit, Fact, MemoryContext, validate_citations,
};
use crate::host::mind::embedding::PendingEmbedding;
use crate::host::mind::memory::{Memory, RecallMode, SHARED_AUDIENCE, Scored, fuse, score, tokens};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json as value};
use std::collections::{HashMap, HashSet};

fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    let audiences: String = row.get("audiences")?;
    let source_context: String = row.get("source_context")?;
    Ok(Memory {
        id: row.get("id")?,
        owner: row.get("owner")?,
        audiences: serde_json::from_str(&audiences).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                audiences.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        content: row.get("content")?,
        importance: row.get("importance")?,
        occurred_at: row.get("occurred_at")?,
        recorded_at: row.get("recorded_at")?,
        last_accessed_at: row.get("last_accessed_at")?,
        memory_revision: row.get::<_, u64>("memory_revision")?,
        content_revision: row.get::<_, u64>("content_revision")?,
        emotion: row.get("emotion")?,
        source_context: serde_json::from_str(&source_context).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                source_context.len(),
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

const COLUMNS: &str = "rowid, id, owner, audiences, content, importance, occurred_at, \
     recorded_at, last_accessed_at, memory_revision, content_revision, emotion, source_context";

/// `COLUMNS` qualified with the `m` alias for the FTS join, where `content`
/// and `rowid` otherwise collide with the external-content table.
const JOINED_COLUMNS: &str = "m.rowid, m.id, m.owner, m.audiences, m.content, m.importance, \
     m.occurred_at, m.recorded_at, m.last_accessed_at, m.memory_revision, \
     m.content_revision, m.emotion, m.source_context";

/// What a caller may write when a memory is first recorded.
pub(crate) struct NewMemory {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) audiences: Vec<String>,
    pub(crate) content: String,
    pub(crate) importance: u8,
    pub(crate) occurred_at: Option<String>,
    pub(crate) source_context: Value,
}

/// One write of `mind_update`. The receipt, the memories and the mind state
/// commit together.
pub(crate) enum MemoryWrite {
    Insert {
        memory: NewMemory,
        supersedes: Option<String>,
    },
    Update(MemoryCorrection),
    Forget(MemoryForget),
}

/// A correction of an existing memory. `expected_revision` is the CAS value
/// the caller read; `None` updates the revision current inside the
/// transaction.
pub(crate) struct MemoryCorrection {
    pub(crate) id: String,
    pub(crate) expected_revision: Option<u64>,
    pub(crate) content: Option<String>,
    pub(crate) importance: Option<u8>,
    pub(crate) supersedes: Option<String>,
}

/// Forgetting is only a lower importance, never a delete.
pub(crate) struct MemoryForget {
    pub(crate) id: String,
    pub(crate) expected_revision: Option<u64>,
}

/// The mind state half of a `mind_update`, already serialised.
pub(crate) struct MindStateChange {
    pub(crate) expected_revision: u64,
    pub(crate) state: String,
}

/// Everything a `mind_update` commits atomically.
pub(crate) struct MindUpdate {
    pub(crate) operation_id: String,
    pub(crate) companion_id: String,
    pub(crate) request_hash: Vec<u8>,
    pub(crate) writes: Vec<MemoryWrite>,
    pub(crate) state: Option<MindStateChange>,
}

fn insert_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    memory: &NewMemory,
    recorded_at: &str,
) -> Result<()> {
    if memory.content.trim().is_empty() || memory.content.chars().count() > 4_000 {
        return Err(HostError::invalid(
            "memory content must be 1-4000 characters",
        ));
    }
    if memory.importance > crate::host::mind::memory::MAX_IMPORTANCE {
        return Err(HostError::invalid("importance must be 0-10"));
    }
    transaction.execute(
        "INSERT INTO memories (id, owner, audiences, content, importance, occurred_at, \
         recorded_at, source_context) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            memory.id,
            memory.owner,
            to_json(&memory.audiences)?,
            memory.content,
            memory.importance,
            memory.occurred_at,
            recorded_at,
            to_json(&memory.source_context)?,
        ],
    )?;
    Ok(())
}

/// Adds a cognitive edge, ignoring one that already exists.
fn add_edge(
    transaction: &rusqlite::Transaction<'_>,
    from_id: &str,
    relation: &str,
    to_id: &str,
    created_at: &str,
) -> Result<()> {
    transaction.execute(
        "INSERT OR IGNORE INTO memory_edges (from_id, relation, to_id, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        params![from_id, relation, to_id, created_at],
    )?;
    Ok(())
}

struct StoredMemory {
    owner: String,
    content: String,
    importance: u8,
    memory_revision: u64,
}

fn read_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<Option<StoredMemory>> {
    let memory = transaction
        .query_row(
            "SELECT owner, content, importance, memory_revision \
             FROM memories WHERE id = ?1",
            [id],
            |row| {
                Ok(StoredMemory {
                    owner: row.get(0)?,
                    content: row.get(1)?,
                    importance: row.get(2)?,
                    memory_revision: row.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(memory)
}

/// Adds a `supersedes` edge only when the old memory belongs to the same
/// companion, then lowers it out of the pinned band.
fn supersede_tx(
    transaction: &rusqlite::Transaction<'_>,
    companion_id: &str,
    new_id: &str,
    old_id: &str,
    stamp: &str,
) -> Result<()> {
    if old_id == new_id {
        return Ok(());
    }
    let exists = transaction
        .query_row(
            "SELECT 1 FROM memories WHERE id = ?1 AND owner = ?2",
            params![old_id, companion_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(());
    }
    add_edge(transaction, new_id, "supersedes", old_id, stamp)?;
    lower_superseded_tx(transaction, companion_id, old_id, new_id)
}

/// A correction that supersedes another memory lowers the old one out of the
/// pinned band; it is never deleted (docs/design/mind.md §4.3).
fn lower_superseded_tx(
    transaction: &rusqlite::Transaction<'_>,
    companion_id: &str,
    old_id: &str,
    new_id: &str,
) -> Result<()> {
    if old_id == new_id {
        return Ok(());
    }
    transaction.execute(
        "UPDATE memories SET importance = MIN(importance, 2), \
         memory_revision = memory_revision + 1 WHERE id = ?1 AND owner = ?2",
        params![old_id, companion_id],
    )?;
    Ok(())
}

impl Store {
    #[allow(dead_code)]
    pub(crate) fn insert_memory(&self, memory: NewMemory) -> Result<Memory> {
        let recorded_at = now();
        let id = memory.id.clone();
        let mut connection = self.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_memory_tx(&transaction, &memory, &recorded_at)?;
        transaction.commit()?;
        drop(connection);
        self.memory(&id)?
            .ok_or_else(|| HostError::storage("memory insert did not persist"))
    }

    pub(crate) fn memory(&self, id: &str) -> Result<Option<Memory>> {
        let connection = self.conn()?;
        let memory = connection
            .query_row(
                &format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"),
                [id],
                row_to_memory,
            )
            .optional()?;
        Ok(memory)
    }

    /// `ControlMemory`: forget lowers importance, remember raises it, share and
    /// unshare edit the audience list. The owner's content is never changed.
    pub(crate) fn control_memory(
        &self,
        id: &str,
        action: MemoryActionKind,
        audience: Option<&str>,
    ) -> Result<Memory> {
        let memory = self
            .memory(id)?
            .ok_or_else(|| HostError::not_found("memory not found"))?;
        let (importance, audiences) = match action {
            MemoryActionKind::Forget => (0u8, memory.audiences.clone()),
            MemoryActionKind::Remember => (memory.importance.max(3), memory.audiences.clone()),
            MemoryActionKind::Share => {
                let audience = audience.unwrap_or(SHARED_AUDIENCE);
                let mut audiences = memory.audiences.clone();
                if !audiences.iter().any(|item| item == audience) {
                    audiences.push(audience.to_owned());
                }
                (memory.importance, audiences)
            }
            MemoryActionKind::Unshare => {
                let audience = audience.unwrap_or(SHARED_AUDIENCE);
                let audiences = memory
                    .audiences
                    .iter()
                    .filter(|item| item.as_str() != audience)
                    .cloned()
                    .collect();
                (memory.importance, audiences)
            }
        };
        if importance == memory.importance && audiences == memory.audiences {
            return Ok(memory);
        }
        let updated = self.conn()?.execute(
            "UPDATE memories SET importance = ?2, audiences = ?3, \
             memory_revision = memory_revision + 1 WHERE id = ?1 AND memory_revision = ?4",
            params![id, importance, to_json(&audiences)?, memory.memory_revision],
        )?;
        if updated == 0 {
            return Err(HostError::conflict("memory changed since it was read"));
        }
        self.memory(id)?
            .ok_or_else(|| HostError::storage("memory control did not persist"))
    }

    /// Marks a memory as actually used. Only explicit tool use or a verified
    /// citation calls this, never a plain search.
    pub(crate) fn cite_memory(&self, id: &str, turn_id: &str, estimated: bool) -> Result<()> {
        let connection = self.conn()?;
        connection.execute(
            "UPDATE memories SET last_accessed_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO memory_edges (from_id, relation, to_id, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                id,
                if estimated {
                    "estimated_cited_in"
                } else {
                    "cited_in"
                },
                turn_id,
                now()
            ],
        )?;
        Ok(())
    }

    /// Ranks the memories `companion` may see. Kept for callers that have no
    /// query vector; it is the lexical half of `recall_hybrid`.
    #[cfg(test)]
    pub(crate) fn recall(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
        limit: usize,
    ) -> Result<Vec<Scored>> {
        Ok(self
            .recall_hybrid(companion, query, mode, &[], limit)?
            .items)
    }

    /// Hybrid recall: FTS5 lexical candidates and vector candidates are fused
    /// with RRF (k=60) and scored with the documented formula. `semantic` is
    /// the ranked memory ids the caller obtained from `CompactVectorIndex`;
    /// when it is empty the result is lexical only and `semantic` is `false`.
    pub(crate) fn recall_hybrid(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
        semantic: &[String],
        limit: usize,
    ) -> Result<RecallOutcome> {
        let lexical = self.lexical_candidates(companion, query, mode)?;
        let semantic = self.memories_by_ids(companion, mode, semantic)?;
        let lexical_ids: Vec<String> = lexical.iter().map(|memory| memory.id.clone()).collect();
        let semantic_ids: Vec<String> = semantic.iter().map(|memory| memory.id.clone()).collect();
        let mut lists: Vec<&[String]> = Vec::new();
        if !lexical_ids.is_empty() {
            lists.push(lexical_ids.as_slice());
        }
        if !semantic_ids.is_empty() {
            lists.push(semantic_ids.as_slice());
        }
        let fused = fuse(&lists, 60.0);
        let relevance: HashMap<&str, f64> = fused
            .iter()
            .map(|(id, value)| (id.as_str(), *value))
            .collect();
        let mut memories: HashMap<String, Memory> = HashMap::new();
        for memory in lexical.into_iter().chain(semantic) {
            memories.entry(memory.id.clone()).or_insert(memory);
        }
        let now_epoch = super::epoch(&now()).unwrap_or(0);
        let mut scored: Vec<Scored> = memories
            .into_values()
            .map(|memory| {
                let relevance = relevance.get(memory.id.as_str()).copied().unwrap_or(0.0);
                let value = score(
                    relevance,
                    memory.importance,
                    memory.last_accessed_at.as_deref(),
                    memory.occurred_at.as_deref(),
                    &memory.recorded_at,
                    now_epoch,
                );
                Scored {
                    memory,
                    score: value,
                    relevance,
                }
            })
            .collect();
        scored.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.memory.importance.cmp(&left.memory.importance))
                .then_with(|| left.memory.id.cmp(&right.memory.id))
        });
        scored.truncate(limit);
        Ok(RecallOutcome {
            semantic: !semantic_ids.is_empty(),
            semantic_ids,
            items: scored,
        })
    }

    /// Reads semantic candidates by id, keeping the caller's rank order and
    /// applying the same visibility and importance filter as the lexical
    /// query.
    fn memories_by_ids(
        &self,
        companion: &str,
        mode: RecallMode,
        ids: &[String],
    ) -> Result<Vec<Memory>> {
        let mut unique = HashSet::new();
        let ids: Vec<&String> = ids
            .iter()
            .filter(|id| !id.is_empty() && unique.insert(id.as_str()))
            .take(64)
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let minimum = mode.minimum_importance();
        let connection = self.conn()?;
        let visible = "(m.owner = ?1 OR m.owner = 'shared' \
             OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = ?2) \
             OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = '*'))";
        let placeholders: Vec<String> = (0..ids.len()).map(|at| format!("?{}", at + 4)).collect();
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {visible} AND m.importance >= ?3 \
             AND m.id IN ({})",
            placeholders.join(", ")
        ))?;
        let mut parameters: Vec<rusqlite::types::Value> = vec![
            rusqlite::types::Value::Text(companion.to_owned()),
            rusqlite::types::Value::Text(companion.to_owned()),
            rusqlite::types::Value::Integer(minimum as i64),
        ];
        for id in &ids {
            parameters.push(rusqlite::types::Value::Text((*id).clone()));
        }
        let rows = statement.query_map(rusqlite::params_from_iter(parameters), row_to_memory)?;
        let mut found: HashMap<String, Memory> = HashMap::new();
        for memory in rows {
            let memory = memory?;
            found.insert(memory.id.clone(), memory);
        }
        Ok(ids
            .into_iter()
            .filter_map(|id| found.remove(id.as_str()))
            .collect())
    }

    /// The pinned core memories (`importance == 10`) within the documented
    /// character budget, newest first.
    pub(crate) fn pinned(&self, companion: &str) -> Result<Vec<Memory>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM memories WHERE owner = ?1 AND importance = 10 \
             \
             ORDER BY COALESCE(last_accessed_at, recorded_at) DESC, rowid DESC"
        ))?;
        let rows = statement.query_map([companion], row_to_memory)?;
        let mut budget = crate::host::mind::memory::PINNED_BUDGET_CHARS;
        let mut pinned = Vec::new();
        for memory in rows {
            let memory = memory?;
            let length = memory.content.chars().count();
            if length > budget {
                break;
            }
            budget -= length;
            pinned.push(memory);
        }
        Ok(pinned)
    }

    /// One page of the companion's own memories, newest first.
    pub(crate) fn list_memories(
        &self,
        companion: &str,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Memory>, Option<i64>)> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM memories WHERE owner = ?1 AND rowid > ?2 \
             ORDER BY rowid LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![companion, cursor.unwrap_or(0), (limit + 1) as i64],
            |row| Ok((row.get::<_, i64>("rowid")?, row_to_memory(row)?)),
        )?;
        let mut items = Vec::new();
        let mut next = None;
        for row in rows {
            let (rowid, memory) = row?;
            if items.len() == limit {
                next = Some(rowid);
                break;
            }
            items.push(memory);
        }
        Ok((items, next))
    }

    fn lexical_candidates(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
    ) -> Result<Vec<Memory>> {
        let minimum = mode.minimum_importance();
        let connection = self.conn()?;
        let visible = "(m.owner = ?1 OR m.owner = 'shared' \
             OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = ?2) \
             OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = '*'))";
        let terms = tokens(query);
        let mut memories = Vec::new();
        if terms.is_empty() {
            let pattern = format!(
                "%{}%",
                query
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM memories m WHERE {visible} AND m.importance >= ?3 \
                 AND m.content LIKE ?4 ESCAPE '\\' ORDER BY m.importance DESC, m.rowid DESC \
                 LIMIT 20"
            ))?;
            let rows = statement.query_map(
                params![companion, companion, minimum, pattern],
                row_to_memory,
            )?;
            for row in rows {
                memories.push(row?);
            }
            return Ok(memories);
        }
        let expression = terms.join(" OR ");
        let mut statement = connection.prepare(&format!(
            "SELECT {JOINED_COLUMNS} FROM memories m JOIN memories_fts f ON m.rowid = f.rowid \
             WHERE {visible} AND m.importance >= ?3 AND memories_fts MATCH ?4 \
             ORDER BY bm25(memories_fts) ASC LIMIT 20"
        ))?;
        let rows = statement.query_map(
            params![companion, companion, minimum, expression],
            row_to_memory,
        )?;
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// The consolidation cursor: the turn the next job starts after.
    pub(crate) fn consolidation_cursor(&self, companion: &str) -> Result<Option<String>> {
        let connection = self.conn()?;
        let cursor = connection
            .query_row(
                "SELECT last_processed_turn_id FROM consolidation_state WHERE companion_id = ?1",
                [companion],
                |row| row.get(0),
            )
            .optional()?;
        Ok(cursor)
    }

    #[allow(dead_code)]
    pub(crate) fn mind_operation(&self, operation_id: &str) -> Result<Option<Value>> {
        let connection = self.conn()?;
        let row: Option<(Vec<u8>, String)> = connection
            .query_row(
                "SELECT request_hash, result FROM mind_operations WHERE operation_id = ?1",
                [operation_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row
            .map(|(_, result)| json(&result))
            .transpose()?
            .map(Some)
            .unwrap_or(None))
    }

    #[allow(dead_code)]
    pub(crate) fn mind_operation_hash(&self, operation_id: &str) -> Result<Option<Vec<u8>>> {
        let connection = self.conn()?;
        let row: Option<Vec<u8>> = connection
            .query_row(
                "SELECT request_hash FROM mind_operations WHERE operation_id = ?1",
                [operation_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(row)
    }

    #[allow(dead_code)]
    pub(crate) fn record_mind_operation(
        &self,
        operation_id: &str,
        companion: &str,
        request_hash: &[u8],
        result: &Value,
    ) -> Result<()> {
        self.conn()?.execute(
            "INSERT OR REPLACE INTO mind_operations \
             (operation_id, companion_id, request_hash, result, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation_id,
                companion,
                request_hash,
                to_json(result)?,
                now()
            ],
        )?;
        Ok(())
    }

    /// Hashes a mind update for idempotency comparison.
    pub(crate) fn mind_hash(&self, value: &Value) -> Vec<u8> {
        digest(to_json(value).unwrap_or_default().as_bytes())
    }

    /// The one atomic write of a `mind_update`: memories, edges, the mind
    /// state and the idempotency receipt commit together, or none do.
    pub(crate) fn apply_mind_update(&self, update: &MindUpdate) -> Result<Value> {
        if update.operation_id.is_empty() || update.operation_id.chars().count() > 128 {
            return Err(HostError::invalid("operation_id must be 1-128 characters"));
        }
        let stamp = now();
        let mut connection = self.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, Vec<u8>, String)> = transaction
            .query_row(
                "SELECT companion_id, request_hash, result FROM mind_operations                  WHERE operation_id = ?1",
                [&update.operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((companion_id, hash, stored)) = existing {
            if companion_id == update.companion_id && hash == update.request_hash {
                return json(&stored);
            }
            return Err(HostError::conflict(
                "operation_id was already used with different content",
            ));
        }
        let mut inserted = Vec::new();
        let mut updated = Vec::new();
        let mut forgotten = Vec::new();
        for write in &update.writes {
            match write {
                MemoryWrite::Insert { memory, supersedes } => {
                    if memory.owner != update.companion_id {
                        return Err(HostError::permission(
                            "a memory can only be recorded for its owner",
                        ));
                    }
                    insert_memory_tx(&transaction, memory, &stamp)?;
                    add_edge(
                        &transaction,
                        &update.companion_id,
                        "remembers",
                        &memory.id,
                        &stamp,
                    )?;
                    if let Some(old) = supersedes {
                        supersede_tx(&transaction, &update.companion_id, &memory.id, old, &stamp)?;
                    }
                    inserted.push(memory.id.clone());
                }
                MemoryWrite::Update(correction) => {
                    let current = read_memory_tx(&transaction, &correction.id)?
                        .ok_or_else(|| HostError::not_found("memory not found"))?;
                    if current.owner != update.companion_id {
                        return Err(HostError::permission("only the owner may correct a memory"));
                    }
                    let expected = correction
                        .expected_revision
                        .unwrap_or(current.memory_revision);
                    if current.memory_revision != expected {
                        return Err(HostError::conflict("memory changed since it was read"));
                    }
                    let content = correction
                        .content
                        .clone()
                        .unwrap_or_else(|| current.content.clone());
                    if content.trim().is_empty() || content.chars().count() > 4_000 {
                        return Err(HostError::invalid(
                            "memory content must be 1-4000 characters",
                        ));
                    }
                    let importance = correction.importance.unwrap_or(current.importance);
                    if importance > crate::host::mind::memory::MAX_IMPORTANCE {
                        return Err(HostError::invalid("importance must be 0-10"));
                    }
                    let content_changed = content != current.content;
                    let importance_changed = importance != current.importance;
                    if content_changed || importance_changed {
                        let changed_content = if content_changed { 1i64 } else { 0i64 };
                        let rows = transaction.execute(
                            "UPDATE memories SET content = ?3, importance = ?4, \
                             content_revision = content_revision + ?5, \
                             memory_revision = memory_revision + 1, last_accessed_at = ?6, \
                             embedding = CASE WHEN ?5 = 1 THEN NULL ELSE embedding END, \
                             embedding_model = CASE WHEN ?5 = 1 THEN NULL ELSE embedding_model END, \
                             embedding_dimensions = CASE WHEN ?5 = 1 THEN NULL ELSE embedding_dimensions END, \
                             embedding_content_revision = CASE WHEN ?5 = 1 THEN NULL \
                                 ELSE embedding_content_revision END \
                             WHERE id = ?1 AND memory_revision = ?2",
                            params![correction.id, expected, content, importance, changed_content, stamp],
                        )?;
                        if rows == 0 {
                            return Err(HostError::conflict("memory changed since it was read"));
                        }
                    }
                    if let Some(old) = &correction.supersedes {
                        supersede_tx(
                            &transaction,
                            &update.companion_id,
                            &correction.id,
                            old,
                            &stamp,
                        )?;
                    }
                    updated.push(correction.id.clone());
                }
                MemoryWrite::Forget(forget) => {
                    let current = read_memory_tx(&transaction, &forget.id)?
                        .ok_or_else(|| HostError::not_found("memory not found"))?;
                    if current.owner != update.companion_id {
                        return Err(HostError::permission("only the owner may forget a memory"));
                    }
                    let expected = forget.expected_revision.unwrap_or(current.memory_revision);
                    if current.memory_revision != expected {
                        return Err(HostError::conflict("memory changed since it was read"));
                    }
                    let rows = transaction.execute(
                        "UPDATE memories SET importance = 0, \
                         memory_revision = memory_revision + 1 \
                         WHERE id = ?1 AND memory_revision = ?2",
                        params![forget.id, expected],
                    )?;
                    if rows == 0 {
                        return Err(HostError::conflict("memory changed since it was read"));
                    }
                    forgotten.push(forget.id.clone());
                }
            }
        }
        let mut state_revision = None;
        if let Some(state) = &update.state {
            let rows = transaction.execute(
                "UPDATE companions SET state = ?2, state_revision = state_revision + 1, \
                 updated_at = ?3 WHERE id = ?1 AND state_revision = ?4",
                params![
                    update.companion_id,
                    state.state,
                    stamp,
                    state.expected_revision
                ],
            )?;
            if rows == 0 {
                return Err(HostError::conflict("mind changed since it was read"));
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
        transaction.execute(
            "INSERT INTO mind_operations \
             (operation_id, companion_id, request_hash, result, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                update.operation_id,
                update.companion_id,
                update.request_hash,
                to_json(&result)?,
                stamp
            ],
        )?;
        transaction.commit()?;
        Ok(result)
    }

    // === User profile =====================================================

    #[allow(dead_code)]
    pub(crate) fn user_profile(&self, id: &str) -> Result<Option<UserProfile>> {
        let connection = self.conn()?;
        let row: Option<(String, String, String, String)> = connection
            .query_row(
                "SELECT name, preferences, static_constraints, updated_at \
                 FROM user_profiles WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        row.map(
            |(name, preferences, static_constraints, updated_at)| -> Result<UserProfile> {
                Ok(UserProfile {
                    id: id.to_owned(),
                    name,
                    preferences: serde_json::from_str(&preferences)
                        .map_err(|_| HostError::storage("stored user preferences are corrupt"))?,
                    static_constraints: serde_json::from_str(&static_constraints)
                        .map_err(|_| HostError::storage("stored user constraints are corrupt"))?,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    /// Seeds or replaces a profile row. The host has no client-facing profile
    /// RPC in this alpha, so this is the write path a setup hook may use.
    #[allow(dead_code)]
    pub(crate) fn upsert_user_profile(&self, profile: &UserProfile) -> Result<()> {
        if profile.name.chars().count() > 128 {
            return Err(HostError::invalid("user name is too long"));
        }
        if profile.preferences.len() > 256
            || profile
                .preferences
                .iter()
                .any(|entry| entry.chars().count() > 2_000)
        {
            return Err(HostError::invalid("user preferences are too large"));
        }
        if profile.static_constraints.len() > 256
            || profile
                .static_constraints
                .iter()
                .any(|entry| entry.chars().count() > 2_000)
        {
            return Err(HostError::invalid("user constraints are too large"));
        }
        self.conn()?.execute(
            "INSERT INTO user_profiles \
             (id, name, preferences, static_constraints, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, \
             preferences = excluded.preferences, \
             static_constraints = excluded.static_constraints, \
             updated_at = excluded.updated_at",
            params![
                profile.id,
                profile.name,
                to_json(&profile.preferences)?,
                to_json(&profile.static_constraints)?,
                now()
            ],
        )?;
        Ok(())
    }

    // === Embeddings =======================================================

    /// The `(id, memory_revision)` signature of one embedding space, used to
    /// decide whether the flat index needs a rebuild. Only rows whose
    /// embedding still matches their content revision are listed.
    pub(crate) fn embedding_revisions(&self, space: &str) -> Result<Vec<(String, u64)>> {
        let dimensions = crate::host::mind::embedding::dimensions_of_space(space);
        let connection = self.conn()?;
        let sql = if dimensions.is_some() {
            "SELECT id, memory_revision FROM memories WHERE embedding_model = ?1 \
             AND embedding_dimensions = ?2 AND embedding IS NOT NULL \
             AND embedding_content_revision = content_revision ORDER BY rowid"
        } else {
            "SELECT id, memory_revision FROM memories WHERE embedding_model = ?1 \
             AND embedding IS NOT NULL AND embedding_content_revision = content_revision \
             ORDER BY rowid"
        };
        let mut statement = connection.prepare(sql)?;
        let rows = match dimensions {
            Some(dimensions) => statement
                .query_map(params![space, dimensions as i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => statement
                .query_map([space], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// The stored BLOB and dimension count of one valid vector.
    pub(crate) fn embedding_blob(&self, id: &str, space: &str) -> Result<Option<(usize, Vec<u8>)>> {
        let dimensions = crate::host::mind::embedding::dimensions_of_space(space);
        let connection = self.conn()?;
        let sql = if dimensions.is_some() {
            "SELECT embedding_dimensions, embedding FROM memories WHERE id = ?1 \
             AND embedding_model = ?2 AND embedding_dimensions = ?3 \
             AND embedding IS NOT NULL AND embedding_content_revision = content_revision"
        } else {
            "SELECT embedding_dimensions, embedding FROM memories WHERE id = ?1 \
             AND embedding_model = ?2 AND embedding IS NOT NULL \
             AND embedding_content_revision = content_revision"
        };
        let row: Option<(i64, Vec<u8>)> = match dimensions {
            Some(dimensions) => connection
                .query_row(sql, params![id, space, dimensions as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?,
            None => connection
                .query_row(sql, params![id, space], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?,
        };
        Ok(row.map(|(dimensions, bytes)| (dimensions as usize, bytes)))
    }

    /// Every space that currently holds at least one valid vector.
    pub(crate) fn embedding_spaces(&self) -> Result<Vec<String>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT embedding_model FROM memories \
             WHERE embedding_model IS NOT NULL AND embedding IS NOT NULL \
             AND embedding_dimensions IS NOT NULL \
             AND embedding_content_revision = content_revision \
             ORDER BY embedding_model",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Memories whose vector is absent or stale for `space`, oldest first.
    pub(crate) fn memories_missing_embedding(
        &self,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        let dimensions = crate::host::mind::embedding::dimensions_of_space(space);
        let connection = self.conn()?;
        let sql = if dimensions.is_some() {
            "SELECT id, content, content_revision FROM memories \
             WHERE (embedding IS NULL OR embedding_model IS NOT ?1 \
             OR embedding_dimensions IS NOT ?2 OR embedding_content_revision IS NULL \
             OR embedding_content_revision != content_revision) \
             ORDER BY rowid LIMIT ?3"
        } else {
            "SELECT id, content, content_revision FROM memories \
             WHERE (embedding IS NULL OR embedding_model IS NOT ?1 \
             OR embedding_content_revision IS NULL \
             OR embedding_content_revision != content_revision) \
             ORDER BY rowid LIMIT ?2"
        };
        let mut statement = connection.prepare(sql)?;
        let rows: Vec<PendingEmbedding> = match dimensions {
            Some(dimensions) => statement
                .query_map(params![space, dimensions as i64, limit as i64], |row| {
                    Ok(PendingEmbedding {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        content_revision: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => statement
                .query_map(params![space, limit as i64], |row| {
                    Ok(PendingEmbedding {
                        id: row.get(0)?,
                        content: row.get(1)?,
                        content_revision: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Writes a generated vector only while the content that was embedded is
    /// still current. `false` means the memory changed meanwhile and the
    /// vector was dropped instead of mixing spaces.
    pub(crate) fn set_embedding(
        &self,
        id: &str,
        content_revision: u64,
        space: &str,
        dimensions: usize,
        bytes: &[u8],
    ) -> Result<bool> {
        let updated = self.conn()?.execute(
            "UPDATE memories SET embedding = ?3, embedding_model = ?4, \
             embedding_dimensions = ?5, embedding_content_revision = content_revision \
             WHERE id = ?1 AND content_revision = ?2",
            params![id, content_revision, bytes, space, dimensions as i64],
        )?;
        Ok(updated > 0)
    }

    /// The candidate memories shown to the consolidation model, newest and
    /// most important first, with the revisions a merge must match.
    pub(crate) fn consolidation_context(
        &self,
        companion: &str,
        limit: usize,
    ) -> Result<Vec<MemoryContext>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT id, memory_revision, importance, content FROM memories \
             WHERE owner = ?1 AND importance >= 2 \
             ORDER BY importance DESC, COALESCE(last_accessed_at, recorded_at) DESC, rowid DESC \
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![companion, limit as i64], |row| {
            Ok(MemoryContext {
                id: row.get(0)?,
                revision: row.get(1)?,
                importance: row.get(2)?,
                content: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Commits one consolidation job. The expected cursor and the memory
    /// revisions captured at job start are checked inside the transaction;
    /// any mismatch rolls back every memory, edge and cursor change.
    pub(crate) fn commit_consolidation(
        &self,
        batch: &ConsolidationBatch,
        facts: &[Fact],
    ) -> Result<ConsolidationCommit> {
        if batch.companion_id.trim().is_empty() || batch.new_cursor.trim().is_empty() {
            return Err(HostError::invalid("consolidation batch is incomplete"));
        }
        let expected: HashMap<&str, u64> = batch
            .context
            .iter()
            .map(|memory| (memory.id.as_str(), memory.revision))
            .collect();
        let mut cited = Vec::new();
        let mut unique = HashSet::new();
        for fact in facts {
            for id in &fact.cited_memory_ids {
                if unique.insert(id.as_str()) {
                    cited.push(id.clone());
                }
            }
        }
        let accepted = validate_citations(&cited, &batch.injected);
        let stamp = now();
        let mut outcome = ConsolidationCommit {
            cursor: batch.new_cursor.clone(),
            ..ConsolidationCommit::default()
        };
        let mut connection = self.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for fact in facts {
            let content = fact.content.trim();
            if content.is_empty() {
                continue;
            }
            if content.chars().count() > 4_000 {
                return Err(HostError::invalid(
                    "consolidated memory content is too long",
                ));
            }
            let importance = fact.importance.unwrap_or(3).min(10);
            let merge = fact.supersedes.as_deref().and_then(|id| {
                expected
                    .get(id)
                    .copied()
                    .map(|revision| (id.to_owned(), revision))
            });
            if let Some((id, revision)) = merge {
                let current = read_memory_tx(&transaction, &id)?
                    .ok_or_else(|| HostError::conflict("the superseded memory is gone"))?;
                if current.owner != batch.companion_id {
                    return Err(HostError::permission(
                        "consolidation cannot merge another companion's memory",
                    ));
                }
                if current.memory_revision != revision {
                    return Err(HostError::conflict(
                        "memory changed while consolidation was running",
                    ));
                }
                let rows = transaction.execute(
                    "UPDATE memories SET content = ?3, importance = ?4, \
                     content_revision = content_revision + 1, \
                     memory_revision = memory_revision + 1, last_accessed_at = ?5, \
                     embedding = NULL, embedding_model = NULL, \
                     embedding_dimensions = NULL, embedding_content_revision = NULL \
                     WHERE id = ?1 AND memory_revision = ?2",
                    params![id, revision, content, importance, stamp],
                )?;
                if rows == 0 {
                    return Err(HostError::conflict(
                        "memory changed while consolidation was running",
                    ));
                }
                outcome.updated.push(id);
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                let audiences = match fact.audience.as_deref() {
                    None => Vec::new(),
                    Some("*") => vec![SHARED_AUDIENCE.to_owned()],
                    Some(audience) => vec![audience.to_owned()],
                };
                insert_memory_tx(
                    &transaction,
                    &NewMemory {
                        id: id.clone(),
                        owner: batch.companion_id.clone(),
                        audiences,
                        content: content.to_owned(),
                        importance,
                        occurred_at: None,
                        source_context: serde_json::json!({
                            "source": "consolidation",
                            "turn_id": batch.new_cursor,
                        }),
                    },
                    &stamp,
                )?;
                add_edge(&transaction, &batch.companion_id, "remembers", &id, &stamp)?;
                if let Some(old) = fact
                    .supersedes
                    .as_deref()
                    .filter(|old| expected.contains_key(old))
                {
                    supersede_tx(&transaction, &batch.companion_id, &id, old, &stamp)?;
                }
                outcome.inserted.push(id);
            }
        }
        for id in &accepted {
            let rows = transaction.execute(
                "UPDATE memories SET last_accessed_at = ?2 WHERE id = ?1",
                params![id, stamp],
            )?;
            if rows > 0 {
                add_edge(
                    &transaction,
                    id,
                    "estimated_cited_in",
                    &batch.new_cursor,
                    &stamp,
                )?;
                outcome.citations.push(id.clone());
            }
        }
        outcome.rejected_citations = cited.len().saturating_sub(outcome.citations.len());
        advance_consolidation_cursor_tx(
            &transaction,
            &batch.companion_id,
            batch.job_start.as_deref(),
            &batch.new_cursor,
            &stamp,
        )?;
        transaction.commit()?;
        Ok(outcome)
    }
}

/// The documented cursor-match check: an update that no longer sees the
/// cursor the job started with aborts the whole transaction.
fn advance_consolidation_cursor_tx(
    transaction: &rusqlite::Transaction<'_>,
    companion_id: &str,
    expected: Option<&str>,
    next: &str,
    stamp: &str,
) -> Result<()> {
    let changed = match expected {
        Some(expected) => transaction.execute(
            "UPDATE consolidation_state SET last_processed_turn_id = ?3, updated_at = ?4 \
             WHERE companion_id = ?1 AND last_processed_turn_id = ?2",
            params![companion_id, expected, next, stamp],
        )?,
        None => transaction.execute(
            "INSERT INTO consolidation_state \
             (companion_id, last_processed_turn_id, updated_at) \
             SELECT ?1, ?2, ?3 WHERE NOT EXISTS \
             (SELECT 1 FROM consolidation_state WHERE companion_id = ?1)",
            params![companion_id, next, stamp],
        )?,
    };
    if changed == 0 {
        return Err(HostError::conflict(
            "the consolidation cursor moved before the commit",
        ));
    }
    Ok(())
}

/// A stored user profile: the facts injected into every dynamic tail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UserProfile {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) preferences: Vec<String>,
    pub(crate) static_constraints: Vec<String>,
    pub(crate) updated_at: String,
}

impl UserProfile {
    #[allow(dead_code)]
    /// The lines `recall.rs` appends to the dynamic tail, without inventing
    /// fields the profile does not have.
    pub(crate) fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.name.trim().is_empty() {
            lines.push(format!("名前: {}", self.name.trim()));
        }
        if !self.preferences.is_empty() {
            lines.push(format!("好み: {}", self.preferences.join("、")));
        }
        if !self.static_constraints.is_empty() {
            lines.push(format!("制約: {}", self.static_constraints.join("、")));
        }
        lines
    }
}

/// The profile lines `recall.rs` injects into the dynamic tail. This alpha
/// has one user profile, so the most recently updated row is the one shown;
/// a host with several users can call `profile_lines_for` instead.
#[allow(dead_code)]
pub(crate) fn profile_lines(store: &Store) -> Vec<String> {
    let Ok(connection) = store.conn() else {
        return Vec::new();
    };
    let id: Option<String> = connection
        .query_row(
            "SELECT id FROM user_profiles ORDER BY updated_at DESC, id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    drop(connection);
    id.and_then(|id| store.user_profile(&id).ok().flatten())
        .map(|profile| profile.lines())
        .unwrap_or_default()
}

/// The profile lines for one explicit user id.
#[allow(dead_code)]
pub(crate) fn profile_lines_for(store: &Store, id: &str) -> Vec<String> {
    store
        .user_profile(id)
        .ok()
        .flatten()
        .map(|profile| profile.lines())
        .unwrap_or_default()
}

/// The result of a hybrid recall: the ranked items plus whether a semantic
/// candidate list was actually used.
#[derive(Clone, Debug)]
pub(crate) struct RecallOutcome {
    pub(crate) items: Vec<Scored>,
    pub(crate) semantic: bool,
    /// The semantic candidates that survived visibility and importance
    /// filtering, in the order the vector index ranked them.
    #[allow(dead_code)]
    pub(crate) semantic_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemoryActionKind {
    Forget,
    Remember,
    Share,
    Unshare,
}

/// The memory detail returned by `mind_read`: the record plus its edges.
pub(crate) fn detail(memory: &Memory, relations: &[(String, String)]) -> Value {
    value!({
        "id": memory.id,
        "owner": memory.owner,
        "audiences": memory.audiences,
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

impl Store {
    pub(crate) fn memory_edges(&self, id: &str) -> Result<Vec<(String, String)>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT relation, to_id FROM memory_edges WHERE from_id = ?1 \
             UNION ALL SELECT relation, from_id FROM memory_edges WHERE to_id = ?1",
        )?;
        let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mind::memory::RecallMode;

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn new(content: &'static str, importance: u8) -> NewMemory {
        NewMemory {
            id: uuid::Uuid::new_v4().to_string(),
            owner: "ene".into(),
            audiences: Vec::new(),
            content: content.into(),
            importance,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    #[test]
    fn recall_finds_content_and_respects_forgetting() {
        let store = store();
        let content: &'static str = "東京都の紅茶の店が好き";
        store.insert_memory(new(content, 6)).unwrap();
        let found = store.recall("ene", "紅茶", RecallMode::Auto, 4).unwrap();
        assert_eq!(found.len(), 1);
        let id = found[0].memory.id.clone();
        store
            .control_memory(&id, MemoryActionKind::Forget, None)
            .unwrap();
        assert!(
            store
                .recall("ene", "紅茶", RecallMode::Auto, 4)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .recall("ene", "紅茶", RecallMode::Explicit, 4)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn audience_edits_are_applied() {
        let store = store();
        let memory = store.insert_memory(new("tea", 5)).unwrap();
        let shared = store
            .control_memory(&memory.id, MemoryActionKind::Share, Some("kai"))
            .unwrap();
        assert!(shared.audiences.contains(&"kai".to_owned()));
        let unshared = store
            .control_memory(&memory.id, MemoryActionKind::Unshare, Some("kai"))
            .unwrap();
        assert!(!unshared.audiences.contains(&"kai".to_owned()));
        assert!(
            store
                .control_memory(&memory.id, MemoryActionKind::Share, None)
                .is_ok()
        );
    }

    #[test]
    fn idempotent_mind_operations_round_trip() {
        let store = store();
        let result = value!({"updated": 1});
        let hash = store.mind_hash(&value!({"op": "update"}));
        store
            .record_mind_operation("op1", "ene", &hash, &result)
            .unwrap();
        assert_eq!(store.mind_operation("op1").unwrap(), Some(result));
        assert_eq!(store.mind_operation_hash("op1").unwrap(), Some(hash));
        assert!(store.mind_operation("missing").unwrap().is_none());
    }

    #[test]
    fn reciprocal_rank_fusion_merges_both_lists() {
        let lexical = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let semantic = vec!["a".to_owned(), "d".to_owned()];
        let fused = fuse(&[lexical.as_slice(), semantic.as_slice()], 60.0);
        // `a` is first in both lists, so it reaches the two-list ceiling;
        // only-semantic `d` still makes the result.
        assert_eq!(fused[0].0, "a");
        assert!((fused[0].1 - 1.0).abs() < 1e-9);
        assert!(fused.iter().any(|(id, _)| id == "d"));
        assert!(
            fused
                .iter()
                .all(|(_, relevance)| (0.0..=1.0).contains(relevance))
        );
    }

    #[test]
    fn hybrid_recall_keeps_semantic_only_candidates() {
        let store = store();
        let lexical = store.insert_memory(new("東京都の紅茶の店", 5)).unwrap();
        let semantic_only = store.insert_memory(new("a favourite drink", 5)).unwrap();
        let outcome = store
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
        let lexical_only = store
            .recall_hybrid("ene", "紅茶", RecallMode::Auto, &[], 8)
            .unwrap();
        assert!(!lexical_only.semantic);
        assert_eq!(lexical_only.items.len(), 1);
    }

    #[test]
    fn forgetting_lowers_the_score_and_excludes_semantic_auto_recall() {
        let store = store();
        let memory = store.insert_memory(new("tea", 6)).unwrap();
        let score_before = score(
            1.0,
            memory.importance,
            None,
            None,
            &memory.recorded_at,
            super::super::epoch(&memory.recorded_at).unwrap_or(0),
        );
        store
            .control_memory(&memory.id, MemoryActionKind::Forget, None)
            .unwrap();
        let forgotten = store.memory(&memory.id).unwrap().unwrap();
        let score_after = score(
            1.0,
            forgotten.importance,
            None,
            None,
            &forgotten.recorded_at,
            super::super::epoch(&forgotten.recorded_at).unwrap_or(0),
        );
        assert!(score_after < score_before);
        assert!(
            store
                .recall_hybrid(
                    "ene",
                    "tea",
                    RecallMode::Auto,
                    std::slice::from_ref(&memory.id),
                    4,
                )
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(
            store
                .recall_hybrid(
                    "ene",
                    "tea",
                    RecallMode::Explicit,
                    std::slice::from_ref(&memory.id),
                    4,
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }

    fn insert_write(content: &str, importance: u8, id: &str) -> MemoryWrite {
        MemoryWrite::Insert {
            memory: NewMemory {
                id: id.into(),
                owner: "ene".into(),
                audiences: Vec::new(),
                content: content.into(),
                importance,
                occurred_at: None,
                source_context: serde_json::json!({}),
            },
            supersedes: None,
        }
    }

    fn apply(
        store: &Store,
        operation: &str,
        hash: &[u8],
        writes: Vec<MemoryWrite>,
        state: Option<MindStateChange>,
    ) -> Result<Value> {
        store.apply_mind_update(&MindUpdate {
            operation_id: operation.into(),
            companion_id: "ene".into(),
            request_hash: hash.to_vec(),
            writes,
            state,
        })
    }

    #[test]
    fn mind_update_corrections_use_cas_and_replay() {
        let store = store();
        let hash = store.mind_hash(&value!({"content": "tea", "v": 1}));
        let result = apply(
            &store,
            "op-1",
            &hash,
            vec![insert_write("tea", 5, "m1")],
            None,
        )
        .unwrap();
        assert_eq!(result["memories"][0], "m1");
        // The same operation and content returns the stored result, not a
        // second insert.
        assert_eq!(
            apply(
                &store,
                "op-1",
                &hash,
                vec![insert_write("tea", 5, "m1")],
                None
            )
            .unwrap(),
            result
        );
        assert_eq!(store.list_memories("ene", None, 10).unwrap().0.len(), 1);
        // A different payload under the same operation id is a conflict.
        let other = store.mind_hash(&value!({"content": "coffee"}));
        assert!(matches!(
            apply(
                &store,
                "op-1",
                &other,
                vec![insert_write("coffee", 5, "m2")],
                None
            ),
            Err(HostError::Conflict(_))
        ));
        // A correction with the revision the caller read succeeds.
        let correct_hash = store.mind_hash(&value!({"content": "green tea"}));
        apply(
            &store,
            "op-2",
            &correct_hash,
            vec![MemoryWrite::Update(MemoryCorrection {
                id: "m1".into(),
                expected_revision: Some(1),
                content: Some("green tea".into()),
                importance: Some(7),
                supersedes: None,
            })],
            None,
        )
        .unwrap();
        let corrected = store.memory("m1").unwrap().unwrap();
        assert_eq!(corrected.content, "green tea");
        assert_eq!(corrected.importance, 7);
        assert_eq!(corrected.memory_revision, 2);
        assert_eq!(corrected.content_revision, 2);
        // A stale revision is rejected without changing anything.
        assert!(matches!(
            apply(
                &store,
                "op-3",
                &store.mind_hash(&value!({"content": "black tea"})),
                vec![MemoryWrite::Update(MemoryCorrection {
                    id: "m1".into(),
                    expected_revision: Some(1),
                    content: Some("black tea".into()),
                    importance: None,
                    supersedes: None,
                })],
                None,
            ),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(store.memory("m1").unwrap().unwrap().content, "green tea");
    }

    #[test]
    fn mind_update_forgets_without_deleting() {
        let store = store();
        let hash = store.mind_hash(&value!({"forget": "m1"}));
        apply(
            &store,
            "op-1",
            &hash,
            vec![insert_write("tea", 6, "m1")],
            None,
        )
        .unwrap();
        apply(
            &store,
            "op-2",
            &store.mind_hash(&value!({"forget": "m1", "v": 2})),
            vec![MemoryWrite::Forget(MemoryForget {
                id: "m1".into(),
                expected_revision: Some(1),
            })],
            None,
        )
        .unwrap();
        let memory = store.memory("m1").unwrap().unwrap();
        assert_eq!(memory.importance, 0);
        assert_eq!(memory.memory_revision, 2);
        assert!(
            store
                .recall("ene", "tea", RecallMode::Auto, 4)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .recall("ene", "tea", RecallMode::Explicit, 4)
                .unwrap()
                .len(),
            1
        );
    }

    fn companion_row(store: &Store) {
        let initial_state = serde_json::to_string(&crate::host::mind::State::default()).unwrap();
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

    #[test]
    fn mind_update_commits_state_with_the_receipt() {
        let store = store();
        companion_row(&store);
        let state = serde_json::to_string(&crate::host::mind::State::default()).unwrap();
        apply(
            &store,
            "op-state",
            &store.mind_hash(&value!({"state": 1})),
            Vec::new(),
            Some(MindStateChange {
                expected_revision: 1,
                state,
            }),
        )
        .unwrap();
        let companion = store.companion("ene").unwrap().unwrap();
        assert_eq!(companion.state_revision, 2);
        // The receipt and the state moved together; a stale retry conflicts.
        assert!(matches!(
            apply(
                &store,
                "op-state-2",
                &store.mind_hash(&value!({"state": 2})),
                Vec::new(),
                Some(MindStateChange {
                    expected_revision: 1,
                    state: serde_json::to_string(&crate::host::mind::State::default()).unwrap(),
                }),
            ),
            Err(HostError::Conflict(_))
        ));
    }

    #[test]
    fn a_failed_state_cas_rolls_back_memories_and_the_receipt() {
        let store = store();
        companion_row(&store);
        let state = serde_json::to_string(&crate::host::mind::State::default()).unwrap();
        assert!(matches!(
            apply(
                &store,
                "op-atomic",
                &store.mind_hash(&value!({"atomic": 1})),
                vec![insert_write("tea", 5, "m1")],
                Some(MindStateChange {
                    expected_revision: 9,
                    state,
                }),
            ),
            Err(HostError::Conflict(_))
        ));
        assert!(store.memory("m1").unwrap().is_none());
        assert!(store.mind_operation("op-atomic").unwrap().is_none());
        assert_eq!(store.companion("ene").unwrap().unwrap().state_revision, 1);
    }

    #[test]
    fn user_profiles_round_trip_and_render_lines() {
        let store = store();
        assert!(store.user_profile("user").unwrap().is_none());
        store
            .upsert_user_profile(&UserProfile {
                id: "user".into(),
                name: "A".into(),
                preferences: vec!["tea".into(), "quiet mornings".into()],
                static_constraints: vec!["no calls before 9".into()],
                updated_at: String::new(),
            })
            .unwrap();
        let profile = store.user_profile("user").unwrap().unwrap();
        assert_eq!(profile.name, "A");
        let lines = profile_lines_for(&store, "user");
        assert_eq!(profile_lines(&store), lines);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|line| line.contains("tea")));
        assert!(lines.iter().any(|line| line.contains("no calls")));
        assert!(profile_lines_for(&store, "missing").is_empty());
    }
}

#[cfg(test)]
mod pinned_tests {
    use super::*;

    #[test]
    fn pinned_memories_survive_embedding_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let memory = store
            .insert_memory(NewMemory {
                id: "pinned-1".into(),
                owner: "ene".into(),
                audiences: Vec::new(),
                content: "the user's name is A".into(),
                importance: 10,
                occurred_at: None,
                source_context: serde_json::json!({}),
            })
            .unwrap();
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE memories SET embedding_content_revision = 1 WHERE id = ?1",
                [&memory.id],
            )
            .unwrap();
        let pinned = store.pinned("ene").unwrap();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].content, "the user's name is A");
    }
}
