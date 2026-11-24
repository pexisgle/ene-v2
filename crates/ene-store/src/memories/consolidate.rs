//! Consolidation: gathering the context to consolidate, then committing the
//! extracted facts and their citations in one transaction.

use super::{
    Batch, COLUMNS, Commit, Fact, Memory, MemoryContext, MemoryRecord, MemoryRepo, Result,
    SHARED_SCOPE, VISIBLE, add_edge, insert_row, now, read_memory_tx, row_to_memory, supersede_tx,
    truncate, validate_citations,
};
use crate::StoreError;
use ene_mind::{RecallMode, in_scope};
use rusqlite::params;
use serde_json::json as value;
use std::collections::{HashMap, HashSet};

impl<'a> MemoryRepo<'a> {
    pub fn consolidation_context(
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
        let connection = self.0.conn()?;
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

    pub fn consolidate(&self, batch: &Batch, facts: &[Fact]) -> Result<Commit> {
        if batch.companion_id.trim().is_empty() {
            return Err(StoreError::Invalid(
                "consolidation batch is incomplete".into(),
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
        let mut connection = self.0.conn()?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for fact in facts {
            apply_consolidated_fact(&transaction, batch, fact, &expected, &stamp, &mut commit)?;
        }
        record_consolidation_citations(&transaction, &accepted, batch.through, &stamp)?;
        transaction.commit()?;
        Ok(commit)
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
        return Err(StoreError::Invalid(
            "consolidated memory content is too long".into(),
        ));
    }
    let importance = fact.importance.unwrap_or(3).min(10);
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
            .ok_or_else(|| StoreError::Conflict("the superseded memory is gone".into()))?;
        if !in_scope(&current.scope, &batch.companion_id) {
            return Err(StoreError::Permission(
                "consolidation cannot merge a memory outside its scope".into(),
            ));
        }
        if current.memory_revision != revision {
            return Err(StoreError::Conflict(
                "memory changed while consolidation was running".into(),
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
            return Err(StoreError::Conflict(
                "memory changed while consolidation was running".into(),
            ));
        }
        commit.updated += 1;
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
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
