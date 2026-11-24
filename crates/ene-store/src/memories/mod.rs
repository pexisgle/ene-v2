//! Memory persistence: insertion, CAS updates, sharing, hybrid recall,
//! consolidation commits and the user profile.

mod consolidate;
mod ops;
#[cfg(test)]
mod tests;

pub use ops::{detail, missing_embeddings_on, profile_lines, write_embedding_on};

use crate::{
    Store, StoreError,
    error::Result,
    json, json_column, to_json,
    util::{digest, epoch, now, truncate},
};
pub use ene_mind::{
    Batch, Commit, Fact, MAX_IMPORTANCE, Memory, MemoryActionKind, MemoryContext, MemoryRecord,
    MindStateChange, MindUpdate, NewMemory, PINNED_BUDGET_CHARS, PendingEmbedding, RecallMode,
    RecallOutcome, SHARED_SCOPE, Scored, StateUpdate, fuse, score, tokens, validate_citations,
};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

const MAX_INJECTED: usize = 64;
const KEEP_INJECTED_TURNS: usize = 64;

const FIELDS: &str = "id, scope, content, importance, occurred_at, recorded_at, \
     last_accessed_at, memory_revision, content_revision, emotion, source_context";

pub(super) const COLUMNS: &str = "rowid, id, scope, content, importance, occurred_at, \
     recorded_at, last_accessed_at, memory_revision, content_revision, emotion, source_context";

const JOINED_COLUMNS: &str = "m.rowid, m.id, m.scope, m.content, m.importance, \
     m.occurred_at, m.recorded_at, m.last_accessed_at, m.memory_revision, \
     m.content_revision, m.emotion, m.source_context";

pub(super) const VISIBLE: &str = "(m.scope = ?1 OR m.scope = 'shared')";

pub(super) fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
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

pub(super) fn check_content(content: &str) -> Result<()> {
    if content.trim().is_empty() || content.chars().count() > 4_000 {
        return Err(StoreError::Invalid(
            "memory content must be 1-4000 characters".into(),
        ));
    }
    Ok(())
}

pub(super) fn check_importance(importance: u8) -> Result<()> {
    if importance > MAX_IMPORTANCE {
        return Err(StoreError::Invalid("importance must be 0-10".into()));
    }
    Ok(())
}

pub(super) fn insert_row(
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

pub(super) fn add_edge(
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

pub(super) struct StoredMemory {
    pub(super) scope: String,
    pub(super) content: String,
    pub(super) importance: u8,
    pub(super) memory_revision: u64,
}

pub(super) fn read_memory_tx(
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

pub(super) fn supersede_tx(
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

/// Repository managing episodic `memories`, `memory_edges`, `memory_injections`,
/// `mind_operations`, and `user_profiles`.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRepo<'a>(&'a Store);

impl<'a> MemoryRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn insert_memory(&self, memory: MemoryRecord) -> Result<Memory> {
        let connection = self.0.conn()?;
        insert_row(&connection, &memory, &now())
    }

    pub fn memory(&self, id: &str) -> Result<Option<Memory>> {
        let connection = self.0.conn()?;
        let memory = connection
            .prepare_cached(&format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"))?
            .query_row([id], row_to_memory)
            .optional()?;
        Ok(memory)
    }

    pub fn control_memory(
        &self,
        id: &str,
        action: MemoryActionKind,
        companion: &str,
    ) -> Result<Memory> {
        let memory = self
            .memory(id)?
            .ok_or_else(|| StoreError::NotFound("memory not found".into()))?;
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
            .0
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
        updated.ok_or_else(|| StoreError::Conflict("memory changed since it was read".into()))
    }

    pub fn cite_memory(&self, id: &str, turn_id: &str, estimated: bool) -> Result<()> {
        let connection = self.0.conn()?;
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

    pub fn recall_hybrid(
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
        let now_epoch = epoch(&now()).unwrap_or(0);
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
        let connection = self.0.conn()?;
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

    pub fn pinned(&self, companion: &str) -> Result<Vec<Memory>> {
        let connection = self.0.conn()?;
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

    pub fn list_memories(
        &self,
        companion: &str,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Memory>, Option<i64>)> {
        let connection = self.0.conn()?;
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
        let connection = self.0.conn()?;
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

    pub fn record_injections(&self, companion: &str, turn_id: &str, ids: &[String]) -> Result<()> {
        let mut connection = self.0.conn()?;
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

    pub fn injected_between(
        &self,
        companion: &str,
        after: Option<u64>,
        through: u64,
    ) -> Result<Vec<String>> {
        let connection = self.0.conn()?;
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

    pub fn mind_hash(&self, value: &Value) -> Vec<u8> {
        digest(to_json(value).unwrap_or_default().as_bytes())
    }
}
