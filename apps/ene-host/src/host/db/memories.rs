//! Memory persistence: insertion, CAS updates, sharing, hybrid recall,
//! consolidation commits and the user profile.
use super::{Result, Store, digest, json, json_column, now, open_connection, to_json};
use crate::host::error::HostError;
use crate::host::json::JsonDoc;
use crate::host::mind::consolidate::{Batch, Commit, Fact, MemoryContext, validate_citations};
use crate::host::mind::embedding::{PendingEmbedding, dimensions_of_space};
use crate::host::mind::memory::{
    MAX_IMPORTANCE, Memory, PINNED_BUDGET_CHARS, RecallMode, SHARED_SCOPE, Scored, fuse, in_scope,
    score, tokens,
};
use rusqlite::{OptionalExtension, Row, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json as value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The most memory ids one turn's injection record keeps.
const MAX_INJECTED: usize = 64;
/// The newest turns whose injection records stay in the database.
const KEEP_INJECTED_TURNS: usize = 64;

const FIELDS: &str = "id, scope, content, importance, occurred_at, recorded_at, \
     last_accessed_at, memory_revision, content_revision, emotion, source_context";

const COLUMNS: &str = "rowid, id, scope, content, importance, occurred_at, \
     recorded_at, last_accessed_at, memory_revision, content_revision, emotion, source_context";

/// `COLUMNS` qualified with the `m` alias for the FTS join, where `content`
/// and `rowid` otherwise collide with the external-content table.
const JOINED_COLUMNS: &str = "m.rowid, m.id, m.scope, m.content, m.importance, \
     m.occurred_at, m.recorded_at, m.last_accessed_at, m.memory_revision, \
     m.content_revision, m.emotion, m.source_context";

/// The visibility predicate every read path shares: the reader's own scope or
/// the shared pool (docs/design/mind.md §3.3). `scope` holds `'shared'` or a
/// companion id, so no value other than the literal widens visibility.
const VISIBLE: &str = "(m.scope = ?1 OR m.scope = 'shared')";

fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get("id")?,
        scope: row.get("scope")?,
        content: row.get("content")?,
        importance: row.get("importance")?,
        occurred_at: row.get("occurred_at")?,
        recorded_at: row.get("recorded_at")?,
        last_accessed_at: row.get("last_accessed_at")?,
        memory_revision: row.get("memory_revision")?,
        content_revision: row.get("content_revision")?,
        emotion: row.get("emotion")?,
        source_context: json_column(
            row,
            "source_context",
            "stored memory source context is corrupt",
        )?,
    })
}

/// What a caller may write when a memory is first recorded.
pub(crate) struct MemoryRecord {
    pub(crate) id: String,
    pub(crate) scope: String,
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
/// A part the caller leaves out keeps its stored value.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateUpdate {
    #[serde(default)]
    pub(crate) emotion: Option<Value>,
    #[serde(default)]
    pub(crate) relationships: Option<Value>,
    #[serde(default)]
    pub(crate) tendencies: Option<Value>,
    #[serde(default)]
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
        return Err(HostError::Invalid(
            ("memory content must be 1-4000 characters").into(),
        ));
    }
    Ok(())
}

fn check_importance(importance: u8) -> Result<()> {
    if importance > MAX_IMPORTANCE {
        return Err(HostError::Invalid(("importance must be 0-10").into()));
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
        "INSERT INTO memories (id, scope, content, importance, occurred_at, \
         recorded_at, source_context) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING {FIELDS}"
    );
    connection
        .prepare_cached(&sql)?
        .query_row(
            params![
                memory.id,
                memory.scope,
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
    transaction
        .prepare_cached(
            "INSERT OR IGNORE INTO memory_edges (from_id, relation, to_id, created_at) \
         VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![from_id, relation, to_id, created_at])?;
    Ok(())
}

struct StoredMemory {
    scope: String,
    content: String,
    importance: u8,
    memory_revision: u64,
}

fn read_memory_tx(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
) -> Result<Option<StoredMemory>> {
    transaction
        .prepare_cached(
            "SELECT scope, content, importance, memory_revision \
             FROM memories WHERE id = ?1",
        )?
        .query_row([id], |row| {
            Ok(StoredMemory {
                scope: row.get(0)?,
                content: row.get(1)?,
                importance: row.get(2)?,
                memory_revision: row.get(3)?,
            })
        })
        .optional()
        .map_err(Into::into)
}

/// Adds a `supersedes` edge only when the old memory is in the companion's
/// scope, then lowers it out of the pinned band.
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
        .prepare_cached("SELECT 1 FROM memories WHERE id = ?1 AND (scope = ?2 OR scope = ?3)")?
        .query_row(params![old_id, companion_id, SHARED_SCOPE], |_| Ok(()))
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
    transaction
        .prepare_cached(
            "UPDATE memories SET importance = MIN(importance, 2), \
         memory_revision = memory_revision + 1 \
         WHERE id = ?1 AND (scope = ?2 OR scope = ?3)",
        )?
        .execute(params![old_id, companion_id, SHARED_SCOPE])?;
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
            .prepare_cached(&format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"))?
            .query_row([id], row_to_memory)
            .optional()?;
        Ok(memory)
    }

    /// `ControlMemory`: forget lowers importance, remember raises it, share
    /// moves the scope to the shared pool and unshare returns it to
    /// `companion`. The memory's content is never changed, and an action that
    /// changes nothing does not bump the revision.
    pub(crate) fn control_memory(
        &self,
        id: &str,
        action: MemoryActionKind,
        companion: &str,
    ) -> Result<Memory> {
        let memory = self
            .memory(id)?
            .ok_or_else(|| HostError::NotFound(("memory not found").into()))?;
        let (importance, scope) = match action {
            MemoryActionKind::Forget => (0u8, memory.scope.clone()),
            MemoryActionKind::Remember => (memory.importance.max(3), memory.scope.clone()),
            MemoryActionKind::Share => (memory.importance, SHARED_SCOPE.to_owned()),
            MemoryActionKind::Unshare => (memory.importance, companion.to_owned()),
        };
        if importance == memory.importance && scope == memory.scope {
            return Ok(memory);
        }
        let updated = self
            .conn()?
            .prepare_cached(&format!(
                "UPDATE memories SET importance = ?2, scope = ?3, \
                     memory_revision = memory_revision + 1 \
                     WHERE id = ?1 AND memory_revision = ?4 RETURNING {FIELDS}"
            ))?
            .query_row(
                params![id, importance, scope, memory.memory_revision],
                row_to_memory,
            )
            .optional()?;
        updated.ok_or_else(|| HostError::Conflict(("memory changed since it was read").into()))
    }

    /// Marks a memory as actually used. Only explicit tool use or a verified
    /// citation calls this, never a plain search.
    pub(crate) fn cite_memory(&self, id: &str, turn_id: &str, estimated: bool) -> Result<()> {
        let connection = self.conn()?;
        connection
            .prepare_cached("UPDATE memories SET last_accessed_at = ?2 WHERE id = ?1")?
            .execute(params![id, now()])?;
        connection
            .prepare_cached(
                "INSERT OR IGNORE INTO memory_edges (from_id, relation, to_id, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                id,
                if estimated {
                    "estimated_cited_in"
                } else {
                    "cited_in"
                },
                turn_id,
                now()
            ])?;
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
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance >= ?2 \
             AND m.id IN (SELECT value FROM json_each(?3))"
        ))?;
        let rows =
            statement.query_map(params![companion, minimum, to_json(&ids)?], row_to_memory)?;
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

    /// The pinned core memories (`importance == 10`) the companion recalls,
    /// within the documented character budget, newest first.
    pub(crate) fn pinned(&self, companion: &str) -> Result<Vec<Memory>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance = 10 \
             ORDER BY COALESCE(m.last_accessed_at, m.recorded_at) DESC, m.rowid DESC"
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

    /// One page of the memories the companion recalls, newest first.
    pub(crate) fn list_memories(
        &self,
        companion: &str,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Memory>, Option<i64>)> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.rowid > ?2 \
             ORDER BY m.rowid LIMIT ?3"
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
            let mut statement = connection.prepare_cached(&format!(
                "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance >= ?2 \
                 AND m.content LIKE ?3 ESCAPE '\\' ORDER BY m.importance DESC, m.rowid DESC \
                 LIMIT 20"
            ))?;
            let rows = statement.query_map(params![companion, minimum, pattern], row_to_memory)?;
            for row in rows {
                memories.push(row?);
            }
            return Ok(memories);
        }
        let expression = terms.join(" OR ");
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {JOINED_COLUMNS} FROM memories m JOIN memories_fts f ON m.rowid = f.rowid \
             WHERE {VISIBLE} AND m.importance >= ?2 AND memories_fts MATCH ?3 \
             ORDER BY bm25(memories_fts) ASC LIMIT 20"
        ))?;
        let rows = statement.query_map(params![companion, minimum, expression], row_to_memory)?;
        for row in rows {
            memories.push(row?);
        }
        Ok(memories)
    }

    /// Records the memory ids the host injected into `turn_id`, one row each,
    /// so the citation guard still knows them after a restart instead of
    /// relying on the volatile in-memory ledger (docs/design/mind.md §4.2).
    pub(crate) fn record_injections(
        &self,
        companion: &str,
        turn_id: &str,
        ids: &[String],
    ) -> Result<()> {
        let mut connection = self.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for id in ids.iter().filter(|id| !id.is_empty()).take(MAX_INJECTED) {
            transaction
                .prepare_cached(
                    "INSERT OR REPLACE INTO memory_injections \
                 (companion_id, turn_id, memory_id, created_at) VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![companion, turn_id, id, now()])?;
        }
        // Keep the ledger bounded: a citation whose injection record is gone is
        // dropped by the guard, which is the safe side of the guard.
        transaction
            .prepare_cached(
                "DELETE FROM memory_injections WHERE companion_id = ?1 AND turn_id NOT IN (\
             SELECT turn_id FROM memory_injections WHERE companion_id = ?1 \
             GROUP BY turn_id ORDER BY CAST(turn_id AS INTEGER) DESC LIMIT ?2)",
            )?
            .execute(params![companion, KEEP_INJECTED_TURNS as i64])?;
        transaction.commit()?;
        Ok(())
    }

    /// The ids injected into the turns of the window `(after, through]`, in
    /// injection order: what the citation guard may accept for that interval.
    /// A turn id the ledger cannot read is skipped rather than guessed at.
    pub(crate) fn injected_between(
        &self,
        companion: &str,
        after: Option<u64>,
        through: u64,
    ) -> Result<Vec<String>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare_cached(
            "SELECT turn_id, memory_id FROM memory_injections \
             WHERE companion_id = ?1 ORDER BY rowid",
        )?;
        let rows = statement.query_map([companion], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for row in rows {
            let (turn, id) = row?;
            let Ok(turn) = turn.parse::<u64>() else {
                continue;
            };
            if turn > through || after.is_some_and(|after| turn <= after) {
                continue;
            }
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Hashes the historical serialized JSON bytes. RFC 8785 changes number
    /// spellings and Unicode key order, invalidating persisted replay receipts.
    pub(crate) fn mind_hash(&self, value: &Value) -> Vec<u8> {
        digest(to_json(value).unwrap_or_default().as_bytes())
    }

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
        super::configure_connection(&connection)?;
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
            &crate::host::db::truncate(query, 2_000),
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
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
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
                return Err(HostError::Invalid(
                    ("consolidated memory content is too long").into(),
                ));
            }
            let importance = fact.importance.unwrap_or(3).min(10);
            // Resurrection guard (docs/design/mind.md §5.2): a superseded
            // memory the user asked to forget (`importance <= 1`) must not be
            // brought back at a higher priority by a late consolidation pass.
            // The fact is dropped, and the rest of the window still commits.
            if let Some(old) = fact.supersedes.as_deref()
                && let Some(current) = read_memory_tx(&transaction, old)?
                && in_scope(&current.scope, &batch.companion_id)
                && current.importance <= 1
                && importance > 1
            {
                tracing::debug!(
                    companion = %batch.companion_id,
                    memory = %old,
                    "consolidation did not revive a forgotten memory"
                );
                continue;
            }
            let merge = fact.supersedes.as_deref().and_then(|id| {
                expected
                    .get(id)
                    .copied()
                    .map(|revision| (id.to_owned(), revision))
            });
            if let Some((id, revision)) = merge {
                let current = read_memory_tx(&transaction, &id)?
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
                updated += 1;
            } else {
                let id = uuid::Uuid::new_v4().to_string();
                // A fact lands in the companion's own scope unless the model
                // asked for the shared pool: one scope column holds one value.
                let scope = if fact.scope.as_deref() == Some(SHARED_SCOPE) {
                    SHARED_SCOPE.to_owned()
                } else {
                    batch.companion_id.clone()
                };
                insert_row(
                    &transaction,
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
            let rows = transaction
                .prepare_cached("UPDATE memories SET last_accessed_at = ?2 WHERE id = ?1")?
                .execute(params![id, stamp])?;
            if rows > 0 {
                add_edge(
                    &transaction,
                    id,
                    "estimated_cited_in",
                    &batch.through.to_string(),
                    &stamp,
                )?;
            }
        }
        transaction.commit()?;
        Ok(Commit { inserted, updated })
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
        store.apply_mind_update(&MindUpdate {
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
        let first = store.insert_memory(new_memory("first", 3)).unwrap();
        let second = store.insert_memory(new_memory("second", 4)).unwrap();
        let connection = store.open_secondary().unwrap();
        let a = 1.0_f32.to_le_bytes();
        let b = 2.0_f32.to_le_bytes();
        for (memory, bytes) in [(&first, &a), (&second, &b), (&first, &b)] {
            assert!(
                write_embedding(
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
            !write_embedding(
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
            write_embedding(
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
            store.embedding_blob(&first.id, "fake:1:v1").unwrap(),
            Some((1, b.to_vec()))
        );
        assert_eq!(
            store.embedding_blob(&second.id, "fake:1:v1").unwrap(),
            Some((1, a.to_vec()))
        );
    }

    #[test]
    fn historical_hash_receipts_replay_after_reopening() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        let fixtures = [
            ("float", r#"{"n":1.0}"#),
            ("unicode", "{\"\u{e000}\":1,\"\u{10000}\":2}"),
        ];
        for (operation, historical) in fixtures {
            let request: Value = serde_json::from_str(historical).unwrap();
            let legacy_hash = digest(historical.as_bytes());
            assert_eq!(store.mind_hash(&request), legacy_hash);
            let canonical = serde_json_canonicalizer::to_string(&request).unwrap();
            assert_ne!(digest(canonical.as_bytes()), legacy_hash);
            store.conn().unwrap().execute(
                "INSERT INTO mind_operations (operation_id, companion_id, request_hash, result, created_at) \
                 VALUES (?1, 'ene', ?2, '{\"replayed\":true}', ?3)",
                params![operation, legacy_hash, now()],
            ).unwrap();
        }
        drop(store);
        let store = Store::open(&path).unwrap();
        for (operation, historical) in fixtures {
            let request: Value = serde_json::from_str(historical).unwrap();
            assert_eq!(
                apply(
                    &store,
                    operation,
                    &store.mind_hash(&request),
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
                Err(HostError::Conflict(_))
            ));
        }
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
            .control_memory(&lexical.id, MemoryActionKind::Forget, "ene")
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
    fn another_companions_scope_is_private_and_sharing_moves_the_row_to_the_pool() {
        let store = store();
        let mut other = new_memory("other companion's secret", 8);
        other.scope = "other".into();
        let other = store.insert_memory(other).unwrap();
        // A scope only one companion names keeps the row out of every other
        // companion's lexical and semantic candidates.
        let private = store
            .recall_hybrid(
                "ene",
                "secret",
                RecallMode::Explicit,
                std::slice::from_ref(&other.id),
                8,
            )
            .unwrap();
        assert!(private.items.is_empty());
        // Sharing rewrites the scope to the shared pool instead of adding an id.
        let shared = store
            .control_memory(&other.id, MemoryActionKind::Share, "other")
            .unwrap();
        assert_eq!(shared.scope, SHARED_SCOPE);
        assert_eq!(shared.memory_revision, 2);
        // Sharing again changes nothing, so it is not a revision bump.
        assert_eq!(
            store
                .control_memory(&other.id, MemoryActionKind::Share, "other")
                .unwrap()
                .memory_revision,
            2
        );
        let recalled = store
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
        // Unsharing returns the row to that companion's own scope.
        let unshared = store
            .control_memory(&other.id, MemoryActionKind::Unshare, "other")
            .unwrap();
        assert_eq!(unshared.scope, "other");
        assert_eq!(unshared.memory_revision, 3);
        assert!(
            store
                .recall_hybrid("ene", "secret", RecallMode::Explicit, &[], 8)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn a_shared_pinned_memory_is_injected_for_every_companion() {
        let store = store();
        let memory = store
            .insert_memory(new_memory("the user's name is A", 10))
            .unwrap();
        assert_eq!(store.pinned("ene").unwrap().len(), 1);
        assert!(store.pinned("other").unwrap().is_empty());
        // Sharing must not unpin the memory for anyone: the shared pool is one
        // scope, so every companion injects and lists it.
        store
            .control_memory(&memory.id, MemoryActionKind::Share, "ene")
            .unwrap();
        assert_eq!(store.pinned("ene").unwrap().len(), 1);
        assert_eq!(store.pinned("other").unwrap().len(), 1);
        assert_eq!(store.list_memories("other", None, 10).unwrap().0.len(), 1);
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
        let relevant = store
            .insert_memory(new_memory("東京都の紅茶の店", 6))
            .unwrap();
        let important = store
            .insert_memory(new_memory("unrelated pinned fact", 10))
            .unwrap();
        let forgotten = store
            .insert_memory(new_memory("紅茶はもう忘れて", 1))
            .unwrap();
        let contexts = store.consolidation_context("ene", "紅茶", &[], 2).unwrap();
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
        // With no lexical or semantic match the important memories fill in.
        let fallback = store.consolidation_context("ene", "zzz", &[], 2).unwrap();
        assert_eq!(fallback.len(), 2);
    }

    #[test]
    fn consolidation_does_not_resurrect_a_forgotten_memory() {
        let store = store();
        let memory = store.insert_memory(new_memory("coffee", 5)).unwrap();
        store
            .control_memory(&memory.id, MemoryActionKind::Forget, "ene")
            .unwrap();
        let forgotten = store.memory(&memory.id).unwrap().unwrap();
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
        let commit = store.consolidate(&batch(7, context), &[fact]).unwrap();
        assert_eq!(commit.inserted, 0);
        assert_eq!(commit.updated, 0);
        let after = store.memory(&memory.id).unwrap().unwrap();
        assert_eq!(after.importance, 0, "the forgotten row stays forgotten");
        assert_eq!(after.content, "coffee");

        // A supersede of a forgotten id that is no longer a candidate is
        // dropped as well: it cannot be re-created as a new memory.
        let commit = store
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
        assert_eq!(store.memory(&memory.id).unwrap().unwrap().importance, 0);
    }

    #[test]
    fn injected_memory_ids_survive_a_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        {
            let store = Store::open(&path).unwrap();
            store
                .record_injections("ene", "5", &["m1".into(), "m2".into()])
                .unwrap();
            store.record_injections("ene", "7", &["m3".into()]).unwrap();
            store
                .record_injections("other", "5", &["m9".into()])
                .unwrap();
        }
        // A new host process reads the verified injection set back.
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.injected_between("ene", Some(5), 7).unwrap(),
            vec!["m3".to_owned()]
        );
        assert_eq!(
            store.injected_between("ene", None, 7).unwrap(),
            vec!["m1".to_owned(), "m2".to_owned(), "m3".to_owned()]
        );
        assert!(
            store
                .injected_between("ene", Some(5), 5)
                .unwrap()
                .is_empty(),
            "after is exclusive, so an empty range has no ids"
        );
        // Another companion's records are not part of this ledger.
        assert_eq!(
            store.injected_between("other", None, 5).unwrap(),
            vec!["m9".to_owned()]
        );
    }
}
