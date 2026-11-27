//! Memory persistence: insertion, CAS updates, sharing, hybrid recall,
//! consolidation commits and the user profile.

mod consolidate;
mod ops;
mod store;
#[cfg(test)]
mod tests;

pub use ops::{detail, profile_lines};
pub(crate) use store::{add_edge, check_content, check_importance, insert_row};

use crate::row::FromRow;
use crate::{
    Store, StoreError,
    error::Result,
    json, to_json,
    util::{digest, epoch, now, truncate},
};
pub use ene_mind::{
    Batch, Commit, Fact, MAX_IMPORTANCE, Memory, MemoryActionKind, MemoryContext, MemoryRecord,
    MindStateChange, MindUpdate, NewMemory, PINNED_BUDGET_CHARS, PendingEmbedding, RecallCandidate,
    RecallMode, RecallOutcome, SHARED_SCOPE, Scored, StateUpdate, score, validate_citations,
};
use rusqlite::{OptionalExtension, params};
use serde_json::Value;
use std::collections::HashSet;

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

fn load_memory(connection: &rusqlite::Connection, id: &str) -> Result<Option<Memory>> {
    Ok(connection
        .prepare_cached(&format!("SELECT {COLUMNS} FROM memories WHERE id = ?1"))?
        .query_row([id], Memory::from_row)
        .optional()?)
}

fn cite_visible_memory(
    transaction: &rusqlite::Transaction<'_>,
    companion: &str,
    id: &str,
    relation: &str,
    turn_id: &str,
    stamp: &str,
) -> Result<bool> {
    let updated = transaction
        .prepare_cached(&format!(
            "UPDATE memories AS m SET last_accessed_at = ?3 WHERE {VISIBLE} AND m.id = ?2"
        ))?
        .execute(params![companion, id, stamp])?
        != 0;
    if updated {
        add_edge(transaction, id, relation, turn_id, stamp)?;
    }
    Ok(updated)
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

    pub fn insert_memory(&self, memory: &MemoryRecord) -> Result<Memory> {
        let connection = self.0.conn();
        insert_row(&connection, memory, &now())
    }

    pub fn memory(&self, id: &str) -> Result<Option<Memory>> {
        load_memory(&self.0.conn(), id)
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
            .conn()
            .prepare_cached(&format!(
                "UPDATE memories SET importance = ?2, scope = ?3, \
                 memory_revision = memory_revision + 1 \
                 WHERE id = ?1 AND memory_revision = ?4 RETURNING {FIELDS}"
            ))?
            .query_row(
                params![id, importance, scope, memory.memory_revision],
                Memory::from_row,
            )
            .optional()?;
        updated.ok_or_else(|| StoreError::Conflict("memory changed since it was read".into()))
    }

    pub fn read_and_cite(
        &self,
        companion: &str,
        id: &str,
        turn_id: &str,
    ) -> Result<(Memory, Vec<(String, String)>)> {
        self.0.write_tx(|transaction| {
            let mut memory = load_memory(transaction, id)?
                .ok_or_else(|| StoreError::NotFound("memory not found".into()))?;
            let stamp = now();
            if !cite_visible_memory(transaction, companion, id, "cited_in", turn_id, &stamp)? {
                return Err(StoreError::Permission(
                    "this memory belongs to another companion".into(),
                ));
            }
            memory.last_accessed_at = Some(stamp);
            let edges = if memory.scope == companion {
                ops::memory_edges(transaction, id)?
            } else {
                Vec::new()
            };
            Ok((memory, edges))
        })
    }

    pub fn recall_hybrid(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
        semantic: &[RecallCandidate],
        limit: usize,
    ) -> Result<RecallOutcome> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(RecallOutcome::default());
        }
        let lexical = self.lexical_candidates(companion, query, mode)?;
        let semantic = self.semantic_memories(companion, mode, semantic)?;
        Ok(RecallOutcome {
            semantic: !semantic.is_empty(),
            items: ene_mind::rank_recall([lexical, semantic], epoch(&now()).unwrap_or(0), limit),
        })
    }

    fn semantic_memories(
        &self,
        companion: &str,
        mode: RecallMode,
        candidates: &[RecallCandidate],
    ) -> Result<Vec<Memory>> {
        let mut unique = HashSet::new();
        let candidates: Vec<_> = candidates
            .iter()
            .filter(|item| !item.id.is_empty() && unique.insert(*item))
            .take(64)
            .collect();
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        self.0.fetch_all(
            &format!(
                "SELECT {JOINED_COLUMNS} FROM json_each(?3) AS candidate CROSS JOIN memories m \
                 WHERE m.id = candidate.value ->> '$.id' \
                 AND m.content_revision = candidate.value ->> '$.content_revision' \
                 AND {VISIBLE} AND m.importance >= ?2 ORDER BY candidate.key"
            ),
            params![companion, mode.minimum_importance(), to_json(&candidates)?],
            Memory::from_row,
        )
    }

    pub fn pinned(&self, companion: &str) -> Result<Vec<Memory>> {
        let connection = self.0.conn();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance = 10 \
             ORDER BY COALESCE(m.last_accessed_at, m.recorded_at) DESC, m.rowid DESC"
        ))?;
        let rows = statement.query_map([companion], Memory::from_row)?;
        let mut budget = PINNED_BUDGET_CHARS;
        let mut pinned = Vec::new();
        for memory in rows {
            let memory = memory?;
            let length = memory.content.chars().count();
            if length > budget {
                continue;
            }
            budget -= length;
            pinned.push(memory);
            if budget == 0 {
                break;
            }
        }
        Ok(pinned)
    }

    pub fn list_memories(
        &self,
        companion: &str,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Memory>, Option<i64>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let connection = self.0.conn();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.rowid > ?2 \
             ORDER BY m.rowid LIMIT ?3"
        ))?;
        let lookahead = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut rows = statement.query(params![companion, cursor.unwrap_or(0), lookahead])?;
        let mut items = Vec::new();
        let mut last = None;
        while let Some(row) = rows.next()? {
            if items.len() == limit {
                return Ok((items, last));
            }
            last = Some(row.get("rowid")?);
            items.push(Memory::from_row(row)?);
        }
        Ok((items, None))
    }

    fn lexical_candidates(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
    ) -> Result<Vec<Memory>> {
        let mut seen = HashSet::new();
        let terms: Vec<_> = ene_mind::recall_terms(query)
            .filter(|term| seen.insert(*term))
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let phrases = terms
            .iter()
            .filter(|term| term.chars().nth(2).is_some())
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        if phrases.is_empty() {
            return self.short_lexical_candidates(companion, mode, &terms);
        }
        self.0.fetch_all(
            &format!(
                "SELECT {JOINED_COLUMNS} FROM memories m JOIN memories_fts f ON m.rowid = f.rowid \
                 WHERE {VISIBLE} AND m.importance >= ?2 AND memories_fts MATCH ?3 \
                 ORDER BY bm25(memories_fts), f.rowid LIMIT 20"
            ),
            params![companion, mode.minimum_importance(), phrases.join(" OR ")],
            Memory::from_row,
        )
    }

    fn short_lexical_candidates(
        &self,
        companion: &str,
        mode: RecallMode,
        terms: &[&str],
    ) -> Result<Vec<Memory>> {
        let pattern = |term: &str| {
            format!(
                "%{}%",
                term.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            )
        };
        let (predicate, parameter) = if let [term] = terms {
            ("m.content LIKE ?3 ESCAPE '\\'", pattern(term))
        } else {
            (
                "EXISTS (SELECT 1 FROM json_each(?3) term WHERE m.content LIKE term.value ESCAPE '\\')",
                to_json(&terms.iter().map(|term| pattern(term)).collect::<Vec<_>>())?,
            )
        };
        self.0.fetch_all(
            &format!(
                "SELECT {COLUMNS} FROM memories m WHERE {VISIBLE} AND m.importance >= ?2 \
                AND {predicate} ORDER BY m.importance DESC, m.rowid DESC LIMIT 20"
            ),
            params![companion, mode.minimum_importance(), parameter],
            Memory::from_row,
        )
    }

    pub fn record_injections(&self, companion: &str, turn_id: &str, ids: &[String]) -> Result<()> {
        let mut connection = self.0.conn();
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
        let connection = self.0.conn();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT i.turn_id, i.memory_id FROM memory_injections i \
             JOIN memories m ON m.id = i.memory_id \
             WHERE i.companion_id = ?1 AND {VISIBLE} ORDER BY i.rowid"
        ))?;
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
