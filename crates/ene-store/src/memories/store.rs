//! Row mapping and the write helpers that own the memories table's shape.
//!
//! Split from `memories::mod` so the SQL column list and the validation that
//! guards every insert sit in one place, apart from recall and consolidation.

use super::{FIELDS, MAX_IMPORTANCE};
use crate::row::FromRow;
use crate::{StoreError, error::Result, json_column, to_json};
use ene_mind::{Memory, MemoryRecord};
use rusqlite::{Row, params};

impl FromRow for Memory {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
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
}

pub(crate) fn check_content(content: &str) -> Result<()> {
    if content.trim().is_empty() || content.chars().count() > 4_000 {
        return Err(StoreError::Invalid(
            "memory content must be 1-4000 characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn check_importance(importance: u8) -> Result<()> {
    if importance > MAX_IMPORTANCE {
        return Err(StoreError::Invalid("importance must be 0-10".into()));
    }
    Ok(())
}

pub(crate) fn insert_row(
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
            Memory::from_row,
        )
        .map_err(Into::into)
}

pub(crate) fn add_edge(
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
