//! Memory rows, FTS recall and the consolidation cursor.
use super::{
    Store,
    companions::{read_companion, write_companion},
    next_revision, now, parse_json,
};
use crate::host::{
    companion::Companion,
    consolidation::Consolidation,
    db::Result,
    error::HostError,
    memory::{Acquisition, Memory, MemoryAction, MindOperation, RecallMode, SHARED_AUDIENCE},
};
use rusqlite::{Connection, OptionalExtension, params};

impl Store {
    pub(crate) fn memory(&self, id: &str) -> Result<Option<Memory>> {
        let id = id.to_owned();
        self.call(move |connection| read_memory(connection, &id).map_err(Into::into))
    }

    #[allow(dead_code)]
    pub(crate) fn memories_for(&self, owner: &str) -> Result<Vec<Memory>> {
        let owner = owner.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {COLUMNS} FROM memories m WHERE m.owner=?1 ORDER BY m.recorded_at DESC"
            ))?;
            statement
                .query_map([&owner], row_to_memory)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    #[allow(dead_code)]
    pub(crate) fn count_memories(&self, owner: &str) -> Result<u64> {
        let owner = owner.to_owned();
        self.call(move |connection| {
            let count: i64 = connection.query_row(
                "SELECT count(*) FROM memories WHERE owner=?1",
                [&owner],
                |row| row.get(0),
            )?;
            Ok(count.max(0) as u64)
        })
    }

    #[allow(dead_code)]
    pub(crate) fn save_memory(&self, memory: &Memory) -> Result<()> {
        let memory = memory.clone();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            write_memory(&transaction, &memory)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub(crate) fn touch_memory(&self, id: &str, at: &str) -> Result<()> {
        let id = id.to_owned();
        let at = at.to_owned();
        self.call(move |connection| {
            connection.execute(
                "UPDATE memories SET last_accessed_at=?2 WHERE id=?1",
                params![id, at],
            )?;
            Ok(())
        })
    }

    pub(crate) fn recall(
        &self,
        viewer: &str,
        query: &str,
        mode: RecallMode,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let viewer = viewer.to_owned();
        let query = query.trim().to_owned();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        self.call(move |connection| {
            let limit = limit.min(64) as i64;
            let importance = match mode {
                RecallMode::Automatic => 2,
                RecallMode::Explicit => 0,
            };
            if let Some(fts) = fts_query(&query) {
                let sql = format!(
                    "SELECT {COLUMNS} FROM memories m \
                     JOIN memories_fts ON memories_fts.rowid = m.rowid \
                     WHERE memories_fts MATCH ?1 \
                       AND (m.owner=?2 OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value=?2 OR value=?3)) \
                       AND m.importance >= ?4 \
                     ORDER BY bm25(memories_fts), m.recorded_at DESC LIMIT ?5"
                );
                query_memories(
                    connection,
                    &sql,
                    params![fts, viewer, SHARED_AUDIENCE, importance, limit],
                )
            } else {
                let sql = format!(
                    "SELECT {COLUMNS} FROM memories m \
                     WHERE (m.owner=?1 OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value=?1 OR value=?2)) \
                       AND m.importance >= ?3 \
                       AND (instr(m.content, ?4) > 0 OR instr(m.description, ?4) > 0 OR instr(m.appraisal, ?4) > 0) \
                     ORDER BY m.importance DESC, m.recorded_at DESC LIMIT ?5"
                );
                query_memories(
                    connection,
                    &sql,
                    params![viewer, SHARED_AUDIENCE, importance, query, limit],
                )
            }
        })
    }

    pub(crate) fn pinned(&self, viewer: &str, limit: usize) -> Result<Vec<Memory>> {
        let viewer = viewer.to_owned();
        self.call(move |connection| {
            let sql = format!(
                "SELECT {COLUMNS} FROM memories m \
                 WHERE m.importance = 10 \
                   AND (m.owner=?1 OR EXISTS (SELECT 1 FROM json_each(m.audiences) WHERE value=?1 OR value=?2)) \
                 ORDER BY COALESCE(m.last_accessed_at, m.recorded_at) DESC LIMIT ?3"
            );
            query_memories(
                connection,
                &sql,
                params![viewer, SHARED_AUDIENCE, limit.min(64) as i64],
            )
        })
    }

    pub(crate) fn control_memory(
        &self,
        companion_id: &str,
        memory_id: &str,
        action: MemoryAction,
    ) -> Result<Memory> {
        let companion_id = companion_id.to_owned();
        let memory_id = memory_id.to_owned();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let mut memory = read_memory(&transaction, &memory_id)?
                .filter(|memory| memory.owner == companion_id)
                .ok_or_else(|| HostError::not_found("memory not found"))?;
            let (importance, audiences) = match action {
                MemoryAction::Forget => (0, memory.audiences.clone()),
                MemoryAction::Remember => (memory.importance.max(3), memory.audiences.clone()),
                MemoryAction::Share => (memory.importance, vec![SHARED_AUDIENCE.to_owned()]),
                MemoryAction::Unshare => (memory.importance, Vec::new()),
            };
            let changed = importance != memory.importance || audiences != memory.audiences;
            if changed {
                memory.importance = importance;
                memory.audiences = audiences;
                memory.memory_revision = next_revision(memory.memory_revision)?;
                memory.updated_at = now();
                write_memory(&transaction, &memory)?;
                let mut companion = read_companion(&transaction, &companion_id)?
                    .ok_or_else(|| HostError::not_found("companion not found"))?;
                companion.memory_revision = next_revision(companion.memory_revision)?;
                companion.revision = next_revision(companion.revision)?;
                companion.updated_at = memory.updated_at.clone();
                write_companion(&transaction, &companion)?;
            }
            transaction.commit()?;
            Ok(memory)
        })
    }

    pub(crate) fn consolidation(&self, companion_id: &str) -> Result<Option<Consolidation>> {
        let companion_id = companion_id.to_owned();
        self.call(move |connection| {
            read_consolidation(connection, &companion_id).map_err(Into::into)
        })
    }

    pub(crate) fn save_consolidation(
        &self,
        companion_id: &str,
        state: &Consolidation,
    ) -> Result<()> {
        let companion_id = companion_id.to_owned();
        let state = state.clone();
        self.call(move |connection| {
            write_consolidation(connection, &companion_id, &state)?;
            Ok(())
        })
    }

    pub(crate) fn mind_operation(
        &self,
        companion_id: &str,
        operation_id: &str,
    ) -> Result<Option<MindOperation>> {
        let companion_id = companion_id.to_owned();
        let operation_id = operation_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT hash, response FROM mind_operations WHERE companion_id=?1 AND operation_id=?2",
                    params![companion_id, operation_id],
                    |row| {
                        Ok(MindOperation {
                            operation_id: String::new(),
                            hash: row.get(0)?,
                            response: row.get(1)?,
                        })
                    },
                )
                .optional()
                .map_err(Into::into)
        })
    }

    /// Applies one `mind_update` and its idempotency record atomically.
    pub(crate) fn apply_mind_update(
        &self,
        previous_revision: u64,
        companion: &Companion,
        memories: &[Memory],
        operation: &MindOperation,
    ) -> Result<()> {
        let companion = companion.clone();
        let memories = memories.to_vec();
        let operation = operation.clone();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let current = read_companion(&transaction, &companion.id)?
                .ok_or_else(|| HostError::not_found("companion not found"))?;
            if current.revision != previous_revision {
                return Err(HostError::conflict("mind revision changed"));
            }
            for memory in &memories {
                write_memory(&transaction, memory)?;
            }
            write_companion(&transaction, &companion)?;
            transaction.execute(
                "INSERT INTO mind_operations(companion_id, operation_id, hash, response) \
                 VALUES (?1,?2,?3,?4) \
                 ON CONFLICT(companion_id, operation_id) DO UPDATE SET hash=excluded.hash, response=excluded.response",
                params![
                    companion.id,
                    operation.operation_id,
                    operation.hash,
                    operation.response
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    /// Applies extracted memories, the companion revision and the cursor.
    pub(crate) fn apply_consolidation(
        &self,
        expected_memory_revision: u64,
        companion: &Companion,
        memories: &[Memory],
        state: &Consolidation,
    ) -> Result<()> {
        let companion = companion.clone();
        let memories = memories.to_vec();
        let state = state.clone();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let current = read_companion(&transaction, &companion.id)?
                .ok_or_else(|| HostError::not_found("companion not found"))?;
            if current.memory_revision != expected_memory_revision {
                return Err(HostError::conflict("memory revision changed"));
            }
            for memory in &memories {
                write_memory(&transaction, memory)?;
            }
            write_companion(&transaction, &companion)?;
            write_consolidation(&transaction, &companion.id, &state)?;
            transaction.commit()?;
            Ok(())
        })
    }
}

const COLUMNS: &str = "m.id, m.owner, m.audiences, m.content, m.description, m.appraisal, \
    m.acquisition, m.importance, m.occurred_at, m.recorded_at, m.last_accessed_at, \
    m.memory_revision, m.content_revision, m.emotion, m.source_context, m.created_by, m.updated_at";

fn read_memory(connection: &Connection, id: &str) -> rusqlite::Result<Option<Memory>> {
    let mut memory = connection
        .query_row(
            &format!("SELECT {COLUMNS} FROM memories m WHERE m.id=?1"),
            [id],
            row_to_memory,
        )
        .optional()?;
    if let Some(memory) = &mut memory {
        let mut statement = connection
            .prepare("SELECT to_id FROM memory_edges WHERE from_id=?1 AND relation='supersedes'")?;
        memory.supersedes = statement
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
    }
    Ok(memory)
}

fn write_memory(connection: &Connection, memory: &Memory) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO memories(id, owner, audiences, content, description, appraisal, acquisition, \
         importance, occurred_at, recorded_at, last_accessed_at, memory_revision, content_revision, \
         emotion, source_context, embedding, embedding_model, embedding_dimensions, \
         embedding_content_revision, created_by, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,NULL,NULL,NULL,NULL,?16,?17) \
         ON CONFLICT(id) DO UPDATE SET owner=excluded.owner, audiences=excluded.audiences, \
         content=excluded.content, description=excluded.description, appraisal=excluded.appraisal, \
         acquisition=excluded.acquisition, importance=excluded.importance, \
         occurred_at=excluded.occurred_at, recorded_at=excluded.recorded_at, \
         last_accessed_at=excluded.last_accessed_at, memory_revision=excluded.memory_revision, \
         content_revision=excluded.content_revision, emotion=excluded.emotion, \
         source_context=excluded.source_context, created_by=excluded.created_by, \
         updated_at=excluded.updated_at",
        params![
            memory.id,
            memory.owner,
            serde_json::to_string(&memory.audiences).map_err(json_error)?,
            memory.content,
            memory.description,
            memory.appraisal,
            memory.acquisition.as_str(),
            memory.importance,
            memory.occurred_at,
            memory.recorded_at,
            memory.last_accessed_at,
            memory.memory_revision,
            memory.content_revision,
            memory.emotion,
            serde_json::to_string(&memory.source_context).map_err(json_error)?,
            memory.created_by,
            memory.updated_at,
        ],
    )?;
    connection.execute(
        "DELETE FROM memory_edges WHERE from_id=?1 AND relation='supersedes'",
        [&memory.id],
    )?;
    for superseded in &memory.supersedes {
        connection.execute(
            "INSERT OR IGNORE INTO memory_edges(from_id, relation, to_id, created_at) \
             VALUES (?1, 'supersedes', ?2, ?3)",
            params![memory.id, superseded, memory.updated_at],
        )?;
    }
    Ok(())
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    let audiences: String = row.get(2)?;
    let acquisition: String = row.get(6)?;
    let source_context: String = row.get(14)?;
    Ok(Memory {
        id: row.get(0)?,
        owner: row.get(1)?,
        audiences: serde_json::from_str(&audiences).map_err(json_error)?,
        content: row.get(3)?,
        description: row.get(4)?,
        appraisal: row.get(5)?,
        acquisition: Acquisition::parse(&acquisition)
            .ok_or_else(|| sql_error("unknown acquisition"))?,
        importance: row.get(7)?,
        occurred_at: row.get(8)?,
        recorded_at: row.get(9)?,
        last_accessed_at: row.get(10)?,
        memory_revision: row.get(11)?,
        content_revision: row.get(12)?,
        emotion: row.get(13)?,
        source_context: parse_json(&source_context).map_err(json_error)?,
        supersedes: Vec::new(),
        created_by: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

fn query_memories(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<Memory>> {
    let mut statement = connection.prepare(sql)?;
    statement
        .query_map(params, row_to_memory)?
        .map(|row| row.map_err(Into::into))
        .collect()
}

fn read_consolidation(
    connection: &Connection,
    companion_id: &str,
) -> rusqlite::Result<Option<Consolidation>> {
    connection
        .query_row(
            "SELECT last_sequence, last_offset, method_revision, state, reason, jobs, \
             model_calls, input_chars, skipped, attempts, updated_at \
             FROM consolidation_state WHERE companion_id=?1",
            [companion_id],
            |row| {
                Ok(Consolidation {
                    last_sequence: row.get(0)?,
                    last_offset: row.get(1)?,
                    method_revision: row.get(2)?,
                    state: row.get(3)?,
                    reason: row.get(4)?,
                    jobs: row.get(5)?,
                    model_calls: row.get(6)?,
                    input_chars: row.get(7)?,
                    skipped: row.get(8)?,
                    attempts: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            },
        )
        .optional()
}

fn write_consolidation(
    connection: &Connection,
    companion_id: &str,
    state: &Consolidation,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO consolidation_state(companion_id, last_sequence, last_offset, \
         method_revision, state, reason, jobs, model_calls, input_chars, skipped, attempts, \
         updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) \
         ON CONFLICT(companion_id) DO UPDATE SET last_sequence=excluded.last_sequence, \
         last_offset=excluded.last_offset, method_revision=excluded.method_revision, \
         state=excluded.state, reason=excluded.reason, jobs=excluded.jobs, \
         model_calls=excluded.model_calls, input_chars=excluded.input_chars, \
         skipped=excluded.skipped, attempts=excluded.attempts, updated_at=excluded.updated_at",
        params![
            companion_id,
            state.last_sequence,
            state.last_offset,
            state.method_revision,
            state.state,
            state.reason,
            state.jobs,
            state.model_calls,
            state.input_chars,
            state.skipped,
            state.attempts,
            state.updated_at,
        ],
    )?;
    Ok(())
}

fn fts_query(query: &str) -> Option<String> {
    const MAX_TERMS: usize = 32;
    let tokens: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|token| token.chars().count() >= 3)
        .take(MAX_TERMS)
        .collect();
    (!tokens.is_empty()).then(|| {
        tokens
            .into_iter()
            .map(phrase)
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

fn phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn json_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(HostError::storage(error.to_string())),
    )
}

fn sql_error(message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(HostError::storage(message)),
    )
}
