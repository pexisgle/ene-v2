//! Consolidation: gathering the context to consolidate, then committing the
//! extracted facts and their citations in one transaction.

use super::ops::update_mind_state;
use super::{
    Batch, Commit, FIELDS, Fact, Memory, MemoryContext, MemoryRecord, MemoryRepo, Result,
    SHARED_SCOPE, VISIBLE, add_edge, cite_visible_memory, insert_row, load_memory, now,
    supersede_tx, truncate, validate_citations,
};
use crate::StoreError;
use crate::row::FromRow;
use ene_mind::{Checkpoint, MemoryVersion, RecallCandidate, RecallMode, Summary};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json as value};
use std::collections::{HashMap, HashSet};

impl<'a> MemoryRepo<'a> {
    pub fn consolidation_context(
        &self,
        companion: &str,
        query: &str,
        semantic: &[RecallCandidate],
        limit: usize,
    ) -> Result<Vec<MemoryContext>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let mut contexts = Vec::new();
        let mut push = |memory: Memory, contexts: &mut Vec<MemoryContext>| {
            if !memory.visible_to(companion) || !seen.insert(memory.id.clone()) {
                return;
            }
            contexts.push(MemoryContext {
                id: memory.id,
                revision: memory.memory_revision,
                importance: memory.importance,
                content: memory.content,
            });
        };
        let retrieved = self.recall_hybrid(
            companion,
            &truncate(query, 2_000),
            RecallMode::Explicit,
            semantic,
            limit.saturating_mul(2),
        )?;
        for scored in retrieved.items {
            push(scored.memory, &mut contexts);
            if contexts.len() >= limit {
                return Ok(contexts);
            }
        }
        let connection = self.0.conn();
        let mut statement = connection.prepare_cached(&format!(
            "SELECT {FIELDS} FROM memories m \
             WHERE {VISIBLE} AND m.importance >= 2 \
             ORDER BY m.importance DESC, COALESCE(m.last_accessed_at, m.recorded_at) DESC, \
             m.rowid DESC LIMIT ?2"
        ))?;
        let rows = statement.query_map(params![companion, limit as i64], Memory::from_row)?;
        for memory in rows {
            push(memory?, &mut contexts);
            if contexts.len() >= limit {
                break;
            }
        }
        Ok(contexts)
    }

    pub fn compaction_checkpoint(&self, companion: &str) -> Result<Option<Checkpoint>> {
        load_checkpoint(&self.0.conn(), companion)
    }

    pub fn consolidate(&self, batch: &Batch, facts: &[Fact]) -> Result<Commit> {
        let checkpoint = &batch.checkpoint;
        let through = checkpoint.summary.through_message_id;
        if checkpoint.summary.text.trim().is_empty()
            || checkpoint.lineage.is_empty()
            || batch.after.is_some_and(|after| after >= through)
        {
            return Err(StoreError::Invalid(
                "consolidation batch is incomplete".into(),
            ));
        }
        self.0
            .write_tx(|transaction| apply_batch(transaction, batch, facts))
    }
}

fn load_checkpoint(
    connection: &rusqlite::Connection,
    companion: &str,
) -> Result<Option<Checkpoint>> {
    Ok(connection
        .prepare_cached("SELECT lineage, through_message_id, summary FROM compaction_checkpoints WHERE companion_id = ?1")?
        .query_row([companion], |row| Ok(Checkpoint {
            lineage: row.get(0)?,
            summary: Summary { through_message_id: row.get(1)?, text: row.get(2)? },
        }))
        .optional()?)
}

fn apply_batch(transaction: &Transaction<'_>, batch: &Batch, facts: &[Fact]) -> Result<Commit> {
    let checkpoint = &batch.checkpoint;
    let owner = transaction
        .prepare_cached("SELECT 1 FROM companions WHERE id = ?1 AND created_at = ?2")?
        .exists(params![batch.companion_id, batch.companion_created_at])?;
    if !owner {
        return Err(StoreError::Conflict(
            "the consolidation companion was retired".into(),
        ));
    }
    if let Some(previous) = load_checkpoint(transaction, &batch.companion_id)?
        && (previous.lineage != checkpoint.lineage
            || Some(previous.summary.through_message_id) != batch.after)
    {
        return Err(StoreError::Conflict(
            "the compaction checkpoint changed".into(),
        ));
    }
    let expected: HashMap<&str, u64> = batch
        .context
        .iter()
        .map(|memory| (memory.id.as_str(), memory.revision))
        .collect();
    let accepted = validate_citations(
        facts
            .iter()
            .flat_map(|fact| fact.cited_memory_ids.iter().map(String::as_str)),
        &batch.injected,
    );
    let mut current = transaction.prepare_cached(&format!(
        "SELECT 1 FROM memories m WHERE {VISIBLE} AND m.id = ?2 AND m.memory_revision = ?3"
    ))?;
    let versions = expected
        .iter()
        .map(|(&id, &revision)| (id, revision))
        .chain(
            accepted
                .iter()
                .map(|memory| (memory.id.as_str(), memory.revision)),
        );
    for (id, revision) in versions {
        if !current.exists(params![batch.companion_id, id, revision])? {
            return Err(StoreError::Conflict(
                "memory changed while consolidation was running".into(),
            ));
        }
    }
    let stamp = now();
    let mut commit = Commit::default();
    record_consolidation_citations(transaction, batch, &accepted, &stamp)?;
    for fact in facts {
        apply_consolidated_fact(transaction, batch, fact, &expected, &stamp, &mut commit)?;
    }
    if let Some(state) = &batch.reflection
        && !update_mind_state(transaction, &batch.companion_id, state, &stamp)?
    {
        tracing::debug!(companion = %batch.companion_id, "consolidation kept the newer mind state");
    }
    transaction.prepare_cached(
        "INSERT INTO compaction_checkpoints (companion_id, lineage, through_message_id, summary)
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT(companion_id) DO UPDATE SET
         lineage = excluded.lineage, through_message_id = excluded.through_message_id,
         summary = excluded.summary",
    )?.execute(params![batch.companion_id, checkpoint.lineage, checkpoint.summary.through_message_id, checkpoint.summary.text])?;
    Ok(commit)
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
    if importance > 1
        && transaction
            .prepare_cached(&format!(
                "SELECT EXISTS (SELECT 1 FROM memories m WHERE {VISIBLE} AND m.importance <= 1 \
                 AND (m.content = ?2 OR m.id = ?3))"
            ))?
            .query_row(
                params![batch.companion_id, content, fact.supersedes],
                |row| row.get::<_, bool>(0),
            )?
    {
        tracing::debug!(
            companion = %batch.companion_id,
            "consolidation did not revive a forgotten memory"
        );
        return Ok(());
    }
    let mut shared = fact.scope.as_deref() == Some(SHARED_SCOPE);
    if let Some(old_id) = fact.supersedes.as_deref() {
        let Some(&version) = expected.get(old_id) else {
            return Ok(());
        };
        let Some(old) =
            load_memory(transaction, old_id)?.filter(|memory| memory.memory_revision == version)
        else {
            return Err(StoreError::Conflict(
                "memory changed while consolidation was running".into(),
            ));
        };
        if old.source_context.get("source").and_then(Value::as_str) == Some("mind_correction") {
            return Ok(());
        }
        shared &= old.scope == SHARED_SCOPE;
    }
    if transaction
        .prepare_cached(&format!(
            "SELECT 1 FROM memories m WHERE ({VISIBLE} OR ?3) AND m.content = ?2 LIMIT 1",
        ))?
        .exists(params![batch.companion_id, content, shared])?
    {
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    let scope = if shared {
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
                "conversation": {
                    "companion_id": batch.companion_id,
                    "lineage": batch.checkpoint.lineage,
                    "after_message_id": batch.after.map(|id| id.to_string()),
                    "through_message_id": batch.checkpoint.summary.through_message_id.to_string(),
                },
            }),
        },
        stamp,
    )?;
    add_edge(transaction, &batch.companion_id, "remembers", &id, stamp)?;
    if let Some(old_id) = &fact.supersedes {
        supersede_tx(transaction, &batch.companion_id, &id, old_id, stamp)?;
        commit.updated += 1;
    }
    commit.inserted += 1;
    Ok(())
}

fn record_consolidation_citations(
    transaction: &rusqlite::Transaction<'_>,
    batch: &Batch,
    accepted: &[&MemoryVersion],
    stamp: &str,
) -> Result<()> {
    let turn = batch.checkpoint.summary.through_message_id.to_string();
    for memory in accepted {
        if !cite_visible_memory(
            transaction,
            &batch.companion_id,
            &memory.id,
            "estimated_cited_in",
            &turn,
            stamp,
        )? {
            return Err(StoreError::Conflict(
                "a cited memory left scope during consolidation".into(),
            ));
        }
    }
    Ok(())
}
