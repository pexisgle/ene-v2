//! Memory persistence: insertion, CAS updates, sharing and candidate lookup.
use super::{Result, Store, digest, json, now, to_json};
use crate::host::error::HostError;
use crate::host::mind::memory::{Memory, RecallMode, SHARED_AUDIENCE, Scored, fuse, score, tokens};
use rusqlite::{OptionalExtension, Row, params};
use serde_json::{Value, json as value};

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

impl Store {
    pub(crate) fn insert_memory(&self, memory: NewMemory) -> Result<Memory> {
        if memory.content.trim().is_empty() || memory.content.chars().count() > 4_000 {
            return Err(HostError::invalid(
                "memory content must be 1-4000 characters",
            ));
        }
        if memory.importance > crate::host::mind::memory::MAX_IMPORTANCE {
            return Err(HostError::invalid("importance must be 0-10"));
        }
        let recorded_at = now();
        let connection = self.conn()?;
        connection.execute(
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
        drop(connection);
        self.memory(&memory.id)?
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

    /// Ranks the memories `companion` may see. Relevance is the fused lexical
    /// rank; semantic ranking is empty until an embedding connection exists.
    pub(crate) fn recall(
        &self,
        companion: &str,
        query: &str,
        mode: RecallMode,
        limit: usize,
    ) -> Result<Vec<Scored>> {
        let lexical = self.lexical_candidates(companion, query, mode)?;
        let rank_of: Vec<String> = lexical.iter().map(|memory| memory.id.clone()).collect();
        let fused = fuse(&[rank_of.as_slice()], 60.0);
        let relevance: std::collections::HashMap<&str, f64> = fused
            .iter()
            .map(|(id, value)| (id.as_str(), *value))
            .collect();
        let now_epoch = super::epoch(&now()).unwrap_or(0);
        let mut scored: Vec<Scored> = lexical
            .into_iter()
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
        Ok(scored)
    }

    /// The pinned core memories (`importance == 10`) within the documented
    /// character budget, newest first.
    pub(crate) fn pinned(&self, companion: &str) -> Result<Vec<Memory>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {COLUMNS} FROM memories WHERE owner = ?1 AND importance = 10 \
             AND embedding_content_revision IS NULL \
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
            "SELECT {COLUMNS} FROM memories m JOIN memories_fts f ON m.rowid = f.rowid \
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
}
