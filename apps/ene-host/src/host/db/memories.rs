//! Memory persistence: insertion, CAS updates, sharing, hybrid recall,
//! consolidation commits and the user profile.
use super::{Result, Store, digest, json, now, to_json};
use crate::host::error::HostError;
use crate::host::json::JsonDoc;
use crate::host::mind::consolidate::{Batch, Commit, Fact, MemoryContext, validate_citations};
use crate::host::mind::embedding::{PendingEmbedding, dimensions_of_space};
use crate::host::mind::memory::{
    MAX_IMPORTANCE, Memory, PINNED_BUDGET_CHARS, RecallMode, SHARED_AUDIENCE, Scored, fuse, score,
    tokens,
};
use rusqlite::{OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as value};
use std::collections::{HashMap, HashSet};

const FIELDS: &str = "id, owner, audiences, content, importance, occurred_at, recorded_at, \
     last_accessed_at, memory_revision, content_revision, emotion, source_context";

const COLUMNS: &str = "rowid, id, owner, audiences, content, importance, occurred_at, \
     recorded_at, last_accessed_at, memory_revision, content_revision, emotion, source_context";

/// `COLUMNS` qualified with the `m` alias for the FTS join, where `content`
/// and `rowid` otherwise collide with the external-content table.
const JOINED_COLUMNS: &str = "m.rowid, m.id, m.owner, m.audiences, m.content, m.importance, \
     m.occurred_at, m.recorded_at, m.last_accessed_at, m.memory_revision, \
     m.content_revision, m.emotion, m.source_context";

/// The visibility predicate shared by the lexical and semantic candidates.
const VISIBLE: &str = "(m.owner = ?1 OR m.owner = 'shared' \
     OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = ?2) \
     OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value = '*'))";

/// A stored JSON column that cannot be parsed is corrupt state, not a bad
/// request: SQLite reports it as a conversion failure.
fn corrupt(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    let audiences: String = row.get("audiences")?;
    let source_context: String = row.get("source_context")?;
    Ok(Memory {
        id: row.get("id")?,
        owner: row.get("owner")?,
        audiences: serde_json::from_str(&audiences).map_err(corrupt)?,
        content: row.get("content")?,
        importance: row.get("importance")?,
        occurred_at: row.get("occurred_at")?,
        recorded_at: row.get("recorded_at")?,
        last_accessed_at: row.get("last_accessed_at")?,
        memory_revision: row.get("memory_revision")?,
        content_revision: row.get("content_revision")?,
        emotion: row.get("emotion")?,
        source_context: serde_json::from_str(&source_context).map_err(corrupt)?,
    })
}

/// What a caller may write when a memory is first recorded.
pub(crate) struct MemoryRecord {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) audiences: Vec<String>,
    pub(crate) content: String,
    pub(crate) importance: u8,
    pub(crate) occurred_at: Option<String>,
    pub(crate) source_context: Value,
}

/// One element of `mind_update`: a new memory, a correction or a forget.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewMemory {
    /// Correct an existing memory instead of recording a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    /// The memory text; a correction may omit it to change only importance.
    #[serde(default)]
    pub(crate) content: String,
    /// 0-10; 10 is injected every turn, 0-1 are effectively forgotten.
    #[serde(default)]
    pub(crate) importance: Option<u8>,
    /// Share with these companions ("*" for every companion).
    #[serde(default)]
    pub(crate) audiences: Vec<String>,
    #[serde(default)]
    pub(crate) occurred_at: Option<String>,
    /// The revision that was read; a stale value is a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expected_revision: Option<u64>,
    /// An existing memory this record supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supersedes: Option<String>,
    /// Forget the addressed memory by lowering its importance to zero.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) forget: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The mind state patch of a `mind_update`; each part is a schema'd document.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateUpdate {
    pub(crate) emotion: Option<Value>,
    pub(crate) relationships: Option<Value>,
    pub(crate) tendencies: Option<Value>,
    pub(crate) concerns: Option<Value>,
}

impl StateUpdate {
    pub(crate) fn apply(&self, state: &mut crate::host::mind::State) {
        if let Some(emotion) = &self.emotion {
            state.emotion = JsonDoc::new("ene.emotion.v1", emotion.clone());
        }
        if let Some(relationships) = &self.relationships {
            state.relationships = JsonDoc::new("ene.relationships.v1", relationships.clone());
        }
        if let Some(tendencies) = &self.tendencies {
            state.tendencies = JsonDoc::new("ene.tendencies.v1", tendencies.clone());
        }
        if let Some(concerns) = &self.concerns {
            state.concerns = JsonDoc::new("ene.concerns.v1", concerns.clone());
        }
    }
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
    pub(crate) memories: Vec<NewMemory>,
    pub(crate) state: Option<MindStateChange>,
}

fn check_content(content: &str) -> Result<()> {
    if content.trim().is_empty() || content.chars().count() > 4_000 {
        return Err(HostError::invalid(
            "memory content must be 1-4000 characters",
        ));
    }
    Ok(())
}

fn check_importance(importance: u8) -> Result<()> {
    if importance > MAX_IMPORTANCE {
        return Err(HostError::invalid("importance must be 0-10"));
    }
    Ok(())
}

/// Inserts one memory and returns the stored row, so no re-SELECT is needed.
fn insert_row(
    connection: &rusqlite::Connection,
    memory: &MemoryRecord,
    recorded_at: &str,
) -> Result<Memory> {
    check_content(&memory.content)?;
    check_importance(memory.importance)?;
    let sql = format!(
        "INSERT INTO memories (id, owner, audiences, content, importance, occurred_at, \
         recorded_at, source_context) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) RETURNING {FIELDS}"
    );
    connection
        .query_row(
            &sql,
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
            row_to_memory,
        )
        .map_err(Into::into)
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
    transaction
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
        .optional()
        .map_err(Into::into)
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
    #[cfg(test)]
    pub(crate) fn insert_memory(&self, memory: MemoryRecord) -> Result<Memory> {
        let connection = self.conn()?;
        insert_row(&connection, &memory, &now())
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
        let updated = self
            .conn()?
            .query_row(
                &format!(
                    "UPDATE memories SET importance = ?2, audiences = ?3, \
                     memory_revision = memory_revision + 1 \
                     WHERE id = ?1 AND memory_revision = ?4 RETURNING {FIELDS}"
                ),
                params![id, importance, to_json(&audiences)?, memory.memory_revision],
                row_to_memory,
            )
            .optional()?;
        updated.ok_or_else(|| HostError::conflict("memory changed since it was read"))
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
        let placeholders: Vec<String> = (0..ids.len()).map(|at| format!("?{}", at + 4)).collect();
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance >= ?3 \
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
             ORDER BY COALESCE(last_accessed_at, recorded_at) DESC, rowid DESC"
        ))?;
        let rows = statement.query_map([companion], row_to_memory)?;
        let mut budget = PINNED_BUDGET_CHARS;
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
                "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance >= ?3 \
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
             WHERE {VISIBLE} AND m.importance >= ?3 AND memories_fts MATCH ?4 \
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
                "SELECT companion_id, request_hash, result FROM mind_operations \
                 WHERE operation_id = ?1",
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
        for memory in &update.memories {
            if memory.forget {
                let Some(id) = &memory.id else {
                    return Err(HostError::invalid(
                        "forget needs the id of the memory to lower",
                    ));
                };
                forget_memory_tx(&transaction, update, id, memory.expected_revision)?;
                forgotten.push(id.clone());
            } else if let Some(id) = &memory.id {
                if memory.content.trim().is_empty()
                    && memory.importance.is_none()
                    && memory.supersedes.is_none()
                {
                    return Err(HostError::invalid(
                        "a correction needs content, importance or supersedes",
                    ));
                }
                correct_memory_tx(&transaction, update, memory, id, &stamp)?;
                updated.push(id.clone());
            } else {
                if memory.content.trim().is_empty() {
                    return Err(HostError::invalid("memory content is required"));
                }
                let id = uuid::Uuid::new_v4().to_string();
                insert_row(
                    &transaction,
                    &MemoryRecord {
                        id: id.clone(),
                        owner: update.companion_id.clone(),
                        audiences: memory.audiences.clone(),
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

    // === Embeddings =======================================================

    /// The `(id, memory_revision)` signature of one embedding space, used to
    /// decide whether the flat index needs a rebuild. Only rows whose
    /// embedding still matches their content revision are listed.
    pub(crate) fn embedding_revisions(&self, space: &str) -> Result<Vec<(String, u64)>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(Vec::new());
        };
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT id, memory_revision FROM memories WHERE embedding_model = ?1 \
             AND embedding_dimensions = ?2 AND embedding IS NOT NULL \
             AND embedding_content_revision = content_revision ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![space, dimensions as i64], |row| {
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
            .query_row(
                "SELECT embedding_dimensions, embedding FROM memories WHERE id = ?1 \
                 AND embedding_model = ?2 AND embedding_dimensions = ?3 \
                 AND embedding IS NOT NULL AND embedding_content_revision = content_revision",
                params![id, space, dimensions as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(dimensions, bytes)| (dimensions as usize, bytes)))
    }

    /// Memories whose vector is absent or stale for `space`, oldest first.
    pub(crate) fn memories_missing_embedding(
        &self,
        space: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbedding>> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(Vec::new());
        };
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT id, content, content_revision FROM memories \
             WHERE (embedding IS NULL OR embedding_model IS NOT ?1 \
             OR embedding_dimensions IS NOT ?2 OR embedding_content_revision IS NULL \
             OR embedding_content_revision != content_revision) \
             ORDER BY rowid LIMIT ?3",
        )?;
        let rows = statement.query_map(params![space, dimensions as i64, limit as i64], |row| {
            Ok(PendingEmbedding {
                id: row.get(0)?,
                content: row.get(1)?,
                content_revision: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
    pub(crate) fn consolidate(&self, batch: &Batch, facts: &[Fact]) -> Result<Commit> {
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
        let mut inserted = 0;
        let mut updated = 0;
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
                updated += 1;
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                let audiences = match fact.audience.as_deref() {
                    None => Vec::new(),
                    Some("*") => vec![SHARED_AUDIENCE.to_owned()],
                    Some(audience) => vec![audience.to_owned()],
                };
                insert_row(
                    &transaction,
                    &MemoryRecord {
                        id: id.clone(),
                        owner: batch.companion_id.clone(),
                        audiences,
                        content: content.to_owned(),
                        importance,
                        occurred_at: None,
                        source_context: value!({
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
                inserted += 1;
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
            }
        }
        advance_consolidation_cursor_tx(
            &transaction,
            &batch.companion_id,
            batch.job_start.as_deref(),
            &batch.new_cursor,
            &stamp,
        )?;
        transaction.commit()?;
        Ok(Commit {
            inserted,
            updated,
            cursor: batch.new_cursor.clone(),
        })
    }

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

fn forget_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    update: &MindUpdate,
    id: &str,
    expected_revision: Option<u64>,
) -> Result<()> {
    let current =
        read_memory_tx(transaction, id)?.ok_or_else(|| HostError::not_found("memory not found"))?;
    if current.owner != update.companion_id {
        return Err(HostError::permission("only the owner may forget a memory"));
    }
    let expected = expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(HostError::conflict("memory changed since it was read"));
    }
    let rows = transaction.execute(
        "UPDATE memories SET importance = 0, \
         memory_revision = memory_revision + 1 \
         WHERE id = ?1 AND memory_revision = ?2",
        params![id, expected],
    )?;
    if rows == 0 {
        return Err(HostError::conflict("memory changed since it was read"));
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
    let current =
        read_memory_tx(transaction, id)?.ok_or_else(|| HostError::not_found("memory not found"))?;
    if current.owner != update.companion_id {
        return Err(HostError::permission("only the owner may correct a memory"));
    }
    let expected = memory.expected_revision.unwrap_or(current.memory_revision);
    if current.memory_revision != expected {
        return Err(HostError::conflict("memory changed since it was read"));
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
            params![id, expected, content, importance, changed_content, stamp],
        )?;
        if rows == 0 {
            return Err(HostError::conflict("memory changed since it was read"));
        }
    }
    if let Some(old) = &memory.supersedes {
        supersede_tx(transaction, &update.companion_id, id, old, stamp)?;
    }
    Ok(())
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
            "INSERT OR IGNORE INTO consolidation_state \
             (companion_id, last_processed_turn_id, updated_at) VALUES (?1, ?2, ?3)",
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

/// The profile lines `recall.rs` injects into the dynamic tail. This alpha has
/// one user profile, so the most recently updated row is the one shown.
pub(crate) fn profile_lines(store: &Store) -> Vec<String> {
    let Ok(connection) = store.conn() else {
        return Vec::new();
    };
    let row: Option<(String, String, String)> = connection
        .query_row(
            "SELECT name, preferences, static_constraints FROM user_profiles \
             ORDER BY updated_at DESC, id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap_or(None);
    let Some((name, preferences, static_constraints)) = row else {
        return Vec::new();
    };
    // A corrupt profile is dropped rather than half-injected.
    let (Ok(preferences), Ok(static_constraints)) = (
        serde_json::from_str::<Vec<String>>(&preferences),
        serde_json::from_str::<Vec<String>>(&static_constraints),
    ) else {
        return Vec::new();
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
    lines
}

/// The result of a hybrid recall: the ranked items plus whether a semantic
/// candidate list was actually used.
#[derive(Clone, Debug)]
pub(crate) struct RecallOutcome {
    pub(crate) items: Vec<Scored>,
    pub(crate) semantic: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn new_memory(content: &str, importance: u8) -> MemoryRecord {
        MemoryRecord {
            id: uuid::Uuid::new_v4().to_string(),
            owner: "ene".into(),
            audiences: Vec::new(),
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
        store.apply_mind_update(&MindUpdate {
            operation_id: operation.into(),
            companion_id: "ene".into(),
            request_hash: hash.to_vec(),
            memories,
            state,
        })
    }

    #[test]
    fn hybrid_recall_fuses_and_forgetting_excludes_auto_but_keeps_explicit() {
        let store = store();
        let lexical = store
            .insert_memory(new_memory("東京都の紅茶の店", 6))
            .unwrap();
        let semantic_only = store
            .insert_memory(new_memory("a favourite drink", 5))
            .unwrap();
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

        store
            .control_memory(&lexical.id, MemoryActionKind::Forget, None)
            .unwrap();
        assert_eq!(store.memory(&lexical.id).unwrap().unwrap().importance, 0);
        // Auto recall drops the forgotten memory from both halves.
        assert!(
            store
                .recall_hybrid(
                    "ene",
                    "紅茶",
                    RecallMode::Auto,
                    std::slice::from_ref(&lexical.id),
                    4,
                )
                .unwrap()
                .items
                .is_empty()
        );
        // An explicit question about the past still reaches it.
        assert_eq!(
            store
                .recall_hybrid("ene", "紅茶", RecallMode::Explicit, &[], 4)
                .unwrap()
                .items
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

    fn state_only(store: &Store, operation: &str, version: i64) -> Result<Value> {
        apply(
            store,
            operation,
            &store.mind_hash(&value!({"state": version})),
            Vec::new(),
            Some(MindStateChange {
                expected_revision: 1,
                state: serde_json::to_string(&crate::host::mind::State::default()).unwrap(),
            }),
        )
    }

    #[test]
    fn mind_update_is_idempotent_and_corrections_forgets_and_state_use_cas() {
        let store = store();
        companion_row(&store);
        let hash = store.mind_hash(&value!({"content": "tea", "v": 1}));
        let write = input("tea", 5);
        let result = apply(&store, "op-1", &hash, vec![write.clone()], None).unwrap();
        let id = result["memories"][0].as_str().unwrap().to_owned();
        // The same operation and content replays the receipt.
        assert_eq!(
            apply(&store, "op-1", &hash, vec![write], None).unwrap(),
            result
        );
        assert_eq!(store.list_memories("ene", None, 10).unwrap().0.len(), 1);
        // The same operation id with another payload is a conflict.
        assert!(matches!(
            apply(
                &store,
                "op-1",
                &store.mind_hash(&value!({"content": "coffee"})),
                vec![input("coffee", 5)],
                None
            ),
            Err(HostError::Conflict(_))
        ));
        // A correction with the revision the caller read succeeds; a stale
        // revision is rejected without changing anything.
        apply(
            &store,
            "op-2",
            &store.mind_hash(&value!({"content": "green tea"})),
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
                &store.mind_hash(&value!({"content": "black tea"})),
                vec![NewMemory {
                    id: Some(id.clone()),
                    content: "black tea".into(),
                    expected_revision: Some(1),
                    ..NewMemory::default()
                }],
                None,
            ),
            Err(HostError::Conflict(_))
        ));
        let corrected = store.memory(&id).unwrap().unwrap();
        assert_eq!(corrected.content, "green tea");
        assert_eq!(corrected.importance, 7);
        assert_eq!(corrected.memory_revision, 2);
        // Forgetting lowers importance and advances the revision, but keeps
        // the record.
        apply(
            &store,
            "op-4",
            &store.mind_hash(&value!({"forget": 1})),
            vec![NewMemory {
                id: Some(id.clone()),
                forget: true,
                expected_revision: Some(2),
                ..NewMemory::default()
            }],
            None,
        )
        .unwrap();
        let forgotten = store.memory(&id).unwrap().unwrap();
        assert_eq!(forgotten.importance, 0);
        assert_eq!(forgotten.memory_revision, 3);
        // The state CAS and the receipt commit together; a stale state
        // revision rolls the whole operation back.
        state_only(&store, "op-5", 1).unwrap();
        assert_eq!(store.companion("ene").unwrap().unwrap().state_revision, 2);
        assert!(matches!(
            state_only(&store, "op-6", 2),
            Err(HostError::Conflict(_))
        ));
        assert_eq!(store.companion("ene").unwrap().unwrap().state_revision, 2);
    }

    #[test]
    fn profile_lines_inject_the_newest_profile() {
        let store = store();
        assert!(profile_lines(&store).is_empty());
        store
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO user_profiles \
                 (id, name, preferences, static_constraints, updated_at) \
                 VALUES ('user', 'A', ?1, ?2, 'now')",
                params![
                    serde_json::to_string(&vec!["tea", "quiet mornings"]).unwrap(),
                    serde_json::to_string(&vec!["no calls before 9"]).unwrap()
                ],
            )
            .unwrap();
        let lines = profile_lines(&store);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|line| line.contains("tea")));
        assert!(lines.iter().any(|line| line.contains("no calls")));
    }
}
