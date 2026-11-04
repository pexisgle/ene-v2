//! Index work, search, consolidation cursor and memory controls. The canonical
//! memory rows stay in `mind_memories`; FTS, vectors and cursors are derived or
//! bookkeeping state that can lag without losing a memory.

use super::mind;
use super::{Error, Result};
use crate::domain::mind::{MemoryRecord, MindUpdateRequest, SourceRef};
use std::collections::HashMap;
use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, Transaction, params};

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct IndexStatus {
    pub state: String,
    pub reason: Option<String>,
    pub pending_fts: u64,
    pub pending_vectors: u64,
    pub embedding_model: Option<String>,
    pub embedding_calls: u64,
    pub embedding_tokens: u64,
    pub updated_at: String,
}

pub(crate) struct ConsolidationOutcome {
    pub result: crate::domain::mind::MindUpdateResult,
    pub status: ConsolidationStatus,
    pub advanced: bool,
}

pub(crate) struct ConsolidationStatus {
    pub last_sequence: u64,
    pub last_offset: u64,
    pub method_revision: String,
    pub state: String,
    pub reason: Option<String>,
    pub jobs: u64,
    pub skipped: u64,
    pub model_calls: u64,
    pub input_chars: u64,
    pub updated_at: String,
}

pub(crate) struct SearchResult {
    pub memories: Vec<MemoryRecord>,
    pub fts: &'static str,
    pub pending: u64,
    pub reason: Option<String>,
}

pub(crate) struct VectorEntry {
    pub memory_id: String,
    pub model: String,
    pub preprocess: String,
    pub vector: Vec<f32>,
}

/// The input preprocessing contract for the built-in embedding capability.
/// It is stored with every vector and required when a vector is searched, so
/// a model change or a preprocessing change cannot silently share a space.
pub(crate) const TEXT_PREPROCESS: &str = "text-v1";

pub(super) fn ensure_index(tx: &Transaction<'_>, companion: &str, now: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO mind_index_status(companion_id,state,updated_at) VALUES (?1,'idle',?2)",
        params![companion, now],
    )?;
    Ok(())
}

pub(super) fn ensure_consolidation(
    tx: &Transaction<'_>,
    companion: &str,
    method: &str,
    now: &str,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO mind_consolidation(companion_id,last_sequence,last_offset,method_revision,state,updated_at) \
         VALUES (?1,0,0,?2,'idle',?3)",
        params![companion, method, now],
    )?;
    Ok(())
}

fn pending_counts(tx: &Transaction<'_>, owner: &str) -> Result<(u64, u64)> {
    let count = |kind: &str| -> Result<u64> {
        Ok(tx.query_row(
            "SELECT COUNT(*) FROM mind_index_pending p JOIN mind_memories m ON m.id=p.memory_id \
             WHERE m.owner=?1 AND p.kind=?2",
            params![owner, kind],
            |row| row.get::<_, i64>(0),
        )? as u64)
    };
    Ok((count("fts")?, count("vector")?))
}

pub(super) fn index_status(db: &mut Connection, companion: &str, now: &str) -> Result<IndexStatus> {
    let tx = db.transaction()?;
    ensure_index(&tx, companion, now)?;
    let (pending_fts, pending_vectors) = pending_counts(&tx, companion)?;
    let status = tx.query_row(
        "SELECT state,reason,embedding_model,embedding_calls,embedding_tokens,updated_at \
         FROM mind_index_status WHERE companion_id=?1",
        [companion],
        |row| {
            Ok(IndexStatus {
                state: row.get(0)?,
                reason: row.get(1)?,
                pending_fts,
                pending_vectors,
                embedding_model: row.get(2)?,
                embedding_calls: row.get::<_, i64>(3)? as u64,
                embedding_tokens: row.get::<_, i64>(4)? as u64,
                updated_at: row.get(5)?,
            })
        },
    )?;
    tx.commit()?;
    Ok(status)
}

fn unix_seconds(value: &str) -> Result<u64> {
    if let Ok(time) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Ok(time.unix_timestamp().max(0) as u64);
    }
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default())
}

pub(super) fn record_embedding(
    db: &mut Connection,
    companion: &str,
    model: &str,
    tokens: u64,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    ensure_index(&tx, companion, now)?;
    tx.execute(
        "UPDATE mind_index_status SET embedding_model=?2,embedding_calls=embedding_calls+1,embedding_tokens=embedding_tokens+?3,updated_at=?4 WHERE companion_id=?1",
        params![companion, model, tokens as i64, now],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn retry_index(db: &mut Connection, now: &str) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute(
        "UPDATE mind_index_pending SET attempts=0,next_attempt_at=0,updated_at=?1",
        [now],
    )?;
    tx.execute(
        "UPDATE mind_index_status SET state='idle',reason=NULL,updated_at=?1",
        [now],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn set_index_state(
    db: &mut Connection,
    companion: &str,
    state: &str,
    reason: Option<&str>,
    model: Option<&str>,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    ensure_index(&tx, companion, now)?;
    tx.execute(
        "UPDATE mind_index_status SET state=?2,reason=?3,embedding_model=COALESCE(?4,embedding_model),updated_at=?5 \
         WHERE companion_id=?1",
        params![companion, state, reason, model, now],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn consolidation_status(
    db: &mut Connection,
    companion: &str,
    method: &str,
    now: &str,
) -> Result<ConsolidationStatus> {
    let tx = db.transaction()?;
    ensure_consolidation(&tx, companion, method, now)?;
    let status = read_consolidation(&tx, companion)?;
    tx.commit()?;
    Ok(status)
}

fn read_consolidation(tx: &Transaction<'_>, companion: &str) -> Result<ConsolidationStatus> {
    Ok(tx.query_row(
        "SELECT last_sequence,method_revision,state,reason,jobs,model_calls,input_chars,updated_at,last_offset,skipped \
         FROM mind_consolidation WHERE companion_id=?1",
        [companion],
        |row| {
            Ok(ConsolidationStatus {
                last_sequence: row.get::<_, i64>(0)? as u64,
                method_revision: row.get(1)?,
                state: row.get(2)?,
                reason: row.get(3)?,
                jobs: row.get::<_, i64>(4)? as u64,
                model_calls: row.get::<_, i64>(5)? as u64,
                input_chars: row.get::<_, i64>(6)? as u64,
                updated_at: row.get(7)?,
                last_offset: row.get::<_, i64>(8)? as u64,
                skipped: row.get::<_, i64>(9)? as u64,
            })
        },
    )?)
}

pub(super) fn set_consolidation_state(
    db: &mut Connection,
    companion: &str,
    method: &str,
    state: &str,
    reason: Option<&str>,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    ensure_consolidation(&tx, companion, method, now)?;
    tx.execute(
        "UPDATE mind_consolidation SET state=?2,reason=?3,method_revision=?4,updated_at=?5 \
         WHERE companion_id=?1",
        params![companion, state, reason, method, now],
    )?;
    tx.commit()?;
    Ok(())
}

pub(super) fn pending_fts(db: &mut Connection, limit: usize, now: &str) -> Result<Vec<String>> {
    let now = unix_seconds(now)?;
    let mut statement = db.prepare(
        "SELECT p.memory_id FROM mind_index_pending p JOIN mind_memories m ON m.id=p.memory_id \
         WHERE p.kind='fts' AND p.next_attempt_at<=?2 ORDER BY p.updated_at LIMIT ?1",
    )?;
    let ids = statement
        .query_map(params![limit as i64, now as i64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// Indexes canonical rows into FTS. A failure rolls back this batch and leaves
/// the pending rows, so the memory itself is already committed and intact.
pub(super) fn index_fts(db: &mut Connection, ids: &[String], now: &str) -> Result<()> {
    let tx = db.transaction()?;
    for id in ids {
        let row = tx
            .query_row(
                "SELECT content,description,appraisal FROM mind_memories WHERE id=?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        tx.execute("DELETE FROM mind_memories_fts WHERE memory_id=?1", [id])?;
        if let Some((content, description, appraisal)) = row {
            tx.execute(
                "INSERT INTO mind_memories_fts(memory_id,content,description,appraisal) VALUES (?1,?2,?3,?4)",
                params![id, content, description, appraisal],
            )?;
        }
        tx.execute(
            "DELETE FROM mind_index_pending WHERE memory_id=?1 AND kind='fts'",
            [id],
        )?;
    }
    tx.execute("UPDATE mind_index_status SET updated_at=?1", [now])?;
    tx.commit()?;
    Ok(())
}

pub(super) fn pending_vectors(
    db: &mut Connection,
    limit: usize,
    now: &str,
) -> Result<Vec<(String, String, String)>> {
    let mut statement = db.prepare(
        "SELECT p.memory_id,m.owner,m.content FROM mind_index_pending p \
         JOIN mind_memories m ON m.id=p.memory_id \
         WHERE p.kind='vector' AND p.next_attempt_at<=?2 ORDER BY p.updated_at LIMIT ?1",
    )?;
    let now = unix_seconds(now)?;
    let rows = statement
        .query_map(params![limit as i64, now as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub(super) fn store_vectors(
    db: &mut Connection,
    entries: &[VectorEntry],
    model: &str,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    for entry in entries {
        let owner: String = tx.query_row(
            "SELECT owner FROM mind_memories WHERE id=?1",
            [&entry.memory_id],
            |row| row.get(0),
        )?;
        let mut bytes = Vec::with_capacity(entry.vector.len() * 4);
        for value in &entry.vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        tx.execute(
            "INSERT INTO mind_vectors(memory_id,model,dimension,preprocess,vector,updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6) \
             ON CONFLICT(memory_id) DO UPDATE SET model=excluded.model,dimension=excluded.dimension,preprocess=excluded.preprocess,vector=excluded.vector,updated_at=excluded.updated_at",
            params![entry.memory_id, entry.model, entry.vector.len() as i64, entry.preprocess, bytes, now],
        )?;
        tx.execute(
            "DELETE FROM mind_index_pending WHERE memory_id=?1 AND kind='vector'",
            [&entry.memory_id],
        )?;
        ensure_index(&tx, &owner, now)?;
        tx.execute(
            "UPDATE mind_index_status SET state='idle',reason=NULL,embedding_model=?2,updated_at=?3 WHERE companion_id=?1",
            params![owner, model, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn mark_index_error(
    db: &mut Connection,
    ids: &[String],
    kind: &str,
    reason: &str,
    permanent: bool,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    let epoch = unix_seconds(now)?;
    for id in ids {
        let attempts: i64 = tx.query_row(
            "SELECT attempts FROM mind_index_pending WHERE memory_id=?1 AND kind=?2",
            params![id, kind],
            |row| row.get(0),
        )?;
        let next = if permanent || attempts + 1 >= 5 {
            4_102_444_800
        } else {
            epoch + [5_u64, 15, 60, 300][attempts.min(3) as usize]
        };
        tx.execute(
            "UPDATE mind_index_pending SET attempts=attempts+1,last_error=?2,updated_at=?3,next_attempt_at=?5 WHERE memory_id=?1 AND kind=?4",
            params![id, reason, now, kind, next as i64],
        )?;
    }
    for row in tx
        .prepare("SELECT DISTINCT m.owner FROM mind_index_pending p JOIN mind_memories m ON m.id=p.memory_id WHERE p.kind=?1")?
        .query_map([kind], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
    {
        ensure_index(&tx, &row, now)?;
        tx.execute(
            "UPDATE mind_index_status SET state='error',reason=?2,updated_at=?3 WHERE companion_id=?1",
            params![row, reason, now],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn visible_sql(
    caller: &str,
    include_forgotten: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> (String, Vec<tokio_rusqlite::rusqlite::types::Value>) {
    let mut sql = format!(
        "(m.owner=?1 OR m.audience='shared') AND (m.recall_preference!='forgotten' OR {})",
        if include_forgotten { 1 } else { 0 }
    );
    let mut values: Vec<tokio_rusqlite::rusqlite::types::Value> = vec![caller.to_owned().into()];
    if let Some(since) = since {
        values.push(since.to_owned().into());
        sql.push_str(&format!(
            " AND COALESCE(m.occurred_at,m.recorded_at)>=?{}",
            values.len()
        ));
    }
    if let Some(until) = until {
        values.push(until.to_owned().into());
        sql.push_str(&format!(
            " AND COALESCE(m.occurred_at,m.recorded_at)<=?{}",
            values.len()
        ));
    }
    (sql, values)
}

fn fts_query(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.chars().count() < 3 {
        return None;
    }
    Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
}

fn fallback_ids(
    db: &Connection,
    caller: &str,
    query: &str,
    limit: usize,
    include_forgotten: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<String>> {
    let (visible, mut values) = visible_sql(caller, include_forgotten, since, until);
    let terms = mind::search_terms(query);
    let mut sql = format!("SELECT m.id FROM mind_memories m WHERE {visible} AND (");
    for (index, term) in terms.iter().enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        let placeholder = values.len() + 1;
        sql.push_str(&format!(
            "(m.content LIKE ?{placeholder} ESCAPE '\\' OR m.description LIKE ?{placeholder} ESCAPE '\\' OR m.appraisal LIKE ?{placeholder} ESCAPE '\\')"
        ));
        values.push(mind::like(term).into());
    }
    sql.push_str(&format!(
        ") ORDER BY m.recorded_at DESC,m.id LIMIT {}",
        limit
    ));
    let mut statement = db.prepare(&sql)?;
    let ids = statement
        .query_map(tokio_rusqlite::rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn fts_ids(
    db: &Connection,
    caller: &str,
    query: &str,
    limit: usize,
    include_forgotten: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> std::result::Result<Vec<String>, tokio_rusqlite::rusqlite::Error> {
    let Some(match_query) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let (visible, mut values) = visible_sql(caller, include_forgotten, since, until);
    values.push(match_query.into());
    let sql = format!(
        "SELECT m.id FROM mind_memories_fts f JOIN mind_memories m ON m.id=f.memory_id \
         WHERE mind_memories_fts MATCH ?{} AND {visible} ORDER BY bm25(mind_memories_fts) LIMIT {limit}",
        values.len()
    );
    let mut statement = db.prepare(&sql)?;
    statement
        .query_map(tokio_rusqlite::rusqlite::params_from_iter(values), |row| {
            row.get::<_, String>(0)
        })?
        .collect()
}

#[allow(clippy::too_many_arguments, clippy::chunks_exact_to_as_chunks)]
fn vector_ids(
    db: &Connection,
    caller: &str,
    model: &str,
    query: &[f32],
    limit: usize,
    include_forgotten: bool,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<String>> {
    let (visible, mut values) = visible_sql(caller, include_forgotten, since, until);
    let model_placeholder = values.len() + 1;
    let preprocess_placeholder = values.len() + 2;
    values.push(model.to_owned().into());
    values.push(TEXT_PREPROCESS.to_owned().into());
    let sql = format!(
        "SELECT v.memory_id,v.vector FROM mind_vectors v JOIN mind_memories m ON m.id=v.memory_id \
         WHERE v.model=?{model_placeholder} AND v.preprocess=?{preprocess_placeholder} AND {visible}"
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement
        .query_map(tokio_rusqlite::rusqlite::params_from_iter(values), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut scored = Vec::with_capacity(rows.len());
    for (id, bytes) in rows {
        let vector = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect::<Vec<_>>();
        if vector.len() != query.len() {
            continue;
        }
        let dot = query.iter().zip(&vector).map(|(a, b)| a * b).sum::<f32>();
        let q = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        let v = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if q == 0.0 || v == 0.0 {
            continue;
        }
        scored.push((id, dot / (q * v)));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(scored.into_iter().take(limit).map(|(id, _)| id).collect())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn search(
    db: &mut Connection,
    caller: &str,
    query: &str,
    query_vector: Option<&[f32]>,
    model: Option<&str>,
    limit: usize,
    include_forgotten: bool,
    since: Option<&str>,
    until: Option<&str>,
    now: &str,
) -> Result<SearchResult> {
    let tx = db.transaction()?;
    ensure_index(&tx, caller, now)?;
    let (pending_fts, pending_vectors) = pending_counts(&tx, caller)?;
    let pending = pending_fts + pending_vectors;
    let fts = fts_ids(&tx, caller, query, 64, include_forgotten, since, until);
    let (fts_ids, fts_state) = match fts {
        Ok(ids) if !ids.is_empty() => (ids, "ok"),
        _ => (
            fallback_ids(&tx, caller, query, limit, include_forgotten, since, until)?,
            "fallback",
        ),
    };
    let semantic = match (query_vector, model) {
        (Some(query_vector), Some(model)) => vector_ids(
            &tx,
            caller,
            model,
            query_vector,
            64,
            include_forgotten,
            since,
            until,
        )?,
        _ => Vec::new(),
    };
    let mut rrf: HashMap<String, f64> = HashMap::new();
    for (rank, id) in fts_ids.iter().enumerate() {
        *rrf.entry(id.clone()).or_default() += 1.0 / (60.0 + rank as f64);
    }
    for (rank, id) in semantic.iter().enumerate() {
        *rrf.entry(id.clone()).or_default() += 1.0 / (60.0 + rank as f64);
    }
    let mut ranked = rrf.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    let ids = ranked
        .into_iter()
        .map(|(id, _)| id)
        .take(limit)
        .collect::<Vec<_>>();
    let memories = hydrate(&tx, caller, &ids)?;
    tx.commit()?;
    Ok(SearchResult {
        memories,
        fts: fts_state,
        pending,
        reason: None,
    })
}

fn hydrate(tx: &Transaction<'_>, caller: &str, ids: &[String]) -> Result<Vec<MemoryRecord>> {
    let mut correction_maps = std::collections::HashMap::new();
    let mut memories = Vec::new();
    for id in ids {
        let row = tx
            .query_row(
                "SELECT id,owner,revision,content,description,appraisal,acquisition,sources,recorded_at,occurred_at,related,supersedes,recall_preference,created_by,audience \
                 FROM mind_memories WHERE id=?1 AND (owner=?2 OR audience='shared')",
                params![id, caller],
                |row| {
                    mind::memory_from_row(row)
                        .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)
                },
            )
            .optional()?;
        let Some(mut memory) = row else {
            continue;
        };
        let superseded = if let Some(map) = correction_maps.get(&memory.owner) {
            map
        } else {
            let map = mind::superseded_by(tx, &memory.owner)?;
            correction_maps.insert(memory.owner.clone(), map);
            correction_maps.get(&memory.owner).expect("inserted")
        };
        memory.superseded_by = superseded.get(id).cloned().unwrap_or_default();
        if memory.owner != caller {
            // A shared memory's private input references never leave its owner.
            memory.sources = vec![crate::domain::mind::SourceRef {
                kind: crate::domain::mind::SourceKind::Memory,
                id: memory.id.clone(),
                entry_id: None,
                speaker: None,
                observed_at: Some(memory.recorded_at.clone()),
            }];
        }
        memories.push(memory);
    }
    Ok(memories)
}

pub(super) fn read_visible(
    db: &mut Connection,
    caller: &str,
    id: &str,
) -> Result<Option<MemoryRecord>> {
    let tx = db.transaction()?;
    let memory = hydrate(&tx, caller, &[id.to_owned()])?.into_iter().next();
    tx.commit()?;
    Ok(memory)
}

pub(super) fn control(
    db: &mut Connection,
    owner: &str,
    id: &str,
    action: &str,
    now: &str,
) -> Result<MemoryRecord> {
    let tx = db.transaction()?;
    let (owner_of, audience, preference, revision) = tx
        .query_row(
            "SELECT owner,audience,recall_preference,revision FROM mind_memories WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| Error::UnknownReference)?;
    if owner_of != owner {
        return Err(Error::UnknownReference);
    }
    let (new_audience, new_preference) = match action {
        "forget" => (audience.clone(), "forgotten".to_owned()),
        "remember" => (audience.clone(), "normal".to_owned()),
        "share" => ("shared".to_owned(), preference.clone()),
        "unshare" => ("private".to_owned(), preference.clone()),
        _ => return Err(Error::InvalidRequest),
    };
    if new_audience == audience && new_preference == preference {
        let memory = hydrate(&tx, owner, &[id.to_owned()])?
            .into_iter()
            .next()
            .ok_or_else(|| Error::UnknownReference)?;
        tx.commit()?;
        return Ok(memory);
    }
    tx.execute(
        "UPDATE mind_memories SET audience=?2,recall_preference=?3,revision=?4,updated_at=?5 WHERE id=?1",
        params![id, new_audience, new_preference, revision + 1, now],
    )?;
    let memory = hydrate(&tx, owner, &[id.to_owned()])?
        .into_iter()
        .next()
        .ok_or_else(|| Error::UnknownReference)?;
    tx.commit()?;
    Ok(memory)
}

/// Applies a consolidation batch and advances the processed-through sequence in
/// the same transaction. A failure or stale revision advances neither.
#[allow(clippy::too_many_arguments)]
pub(super) fn skip_consolidation(
    db: &mut Connection,
    owner: &str,
    sequence: u64,
    offset: u64,
    method: &str,
    now: &str,
) -> Result<()> {
    let tx = db.transaction()?;
    ensure_consolidation(&tx, owner, method, now)?;
    let status = read_consolidation(&tx, owner)?;
    if (sequence, offset) <= (status.last_sequence, status.last_offset) {
        tx.commit()?;
        return Ok(());
    }
    tx.execute(
        "UPDATE mind_consolidation SET last_sequence=?2,last_offset=?3,skipped=skipped+1,updated_at=?4 WHERE companion_id=?1",
        params![owner, sequence as i64, offset as i64, now],
    )?;
    tx.commit()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn consolidate(
    db: &mut Connection,
    owner: &str,
    operation_id: &str,
    request_hash: &str,
    request: &MindUpdateRequest,
    sources: &[SourceRef],
    through_sequence: u64,
    through_offset: u64,
    method: &str,
    model_calls: u64,
    input_chars: u64,
    now: &str,
) -> Result<ConsolidationOutcome> {
    let tx = db.transaction()?;
    ensure_consolidation(&tx, owner, method, now)?;
    let status = read_consolidation(&tx, owner)?;
    if (status.last_sequence, status.last_offset) >= (through_sequence, through_offset) {
        let existing = tx
            .query_row(
                "SELECT response FROM mind_operations WHERE owner=?1 AND operation_id=?2",
                params![owner, operation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let result = if let Some(response) = existing {
            serde_json::from_str(&response).map_err(Error::Json)?
        } else {
            let revision = super::mind::revision(&tx, owner)?;
            crate::domain::mind::MindUpdateResult {
                operation_id: operation_id.into(),
                revision,
                changed: false,
                memories: Vec::new(),
            }
        };
        tx.commit()?;
        return Ok(ConsolidationOutcome {
            result,
            status,
            advanced: false,
        });
    }
    let result = super::mind::apply_tx(
        &tx,
        owner,
        operation_id,
        request_hash,
        request,
        sources,
        "consolidation",
        now,
    )?;
    tx.execute(
        "UPDATE mind_consolidation SET last_sequence=?2,last_offset=?3,method_revision=?4,state='idle',reason=NULL, \
         jobs=jobs+1,model_calls=model_calls+?5,input_chars=input_chars+?6,updated_at=?7 \
         WHERE companion_id=?1",
        params![owner, through_sequence as i64, through_offset as i64, method, model_calls as i64, input_chars as i64, now],
    )?;
    let status = read_consolidation(&tx, owner)?;
    tx.commit()?;
    Ok(ConsolidationOutcome {
        result,
        status,
        advanced: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mind::{Acquisition, NewMemory, SourceKind};
    const METHOD_REVISION: &str = "test-method";
    use crate::infrastructure::sqlite::Store;

    fn source(kind: SourceKind, id: &str) -> SourceRef {
        SourceRef {
            kind,
            id: id.into(),
            entry_id: Some(format!("entry-{id}")),
            speaker: Some("user".into()),
            observed_at: Some("2026-01-01T00:00:00Z".into()),
        }
    }

    fn memory(content: &str) -> NewMemory {
        NewMemory {
            content: content.into(),
            description: "一緒に作った".into(),
            appraisal: "楽しかった".into(),
            acquisition: Acquisition::DirectExperience,
            supersedes: Vec::new(),
            sources: Vec::new(),
            occurred_at: None,
        }
    }

    fn request(revision: u64, memories: Vec<NewMemory>) -> MindUpdateRequest {
        MindUpdateRequest {
            expected_revision: revision,
            memories,
            state: None,
        }
    }

    async fn seed(store: &Store, owner: &str, content: &str) -> String {
        let owner = owner.to_owned();
        let content = content.to_owned();
        store
            .run(move |db| {
                let result = mind::update(
                    db,
                    &owner,
                    &format!("op-{owner}-{content}"),
                    "hash",
                    &request(1, vec![memory(&content)]),
                    &[source(SourceKind::Input, "request-1")],
                    "parent",
                    "2026-01-01T00:00:00Z",
                )?;
                Ok(result.memories[0].id.clone())
            })
            .await
            .unwrap()
    }

    async fn index(store: &Store) {
        store
            .run(|db| {
                let ids = pending_fts(db, 64, "now")?;
                index_fts(db, &ids, "2026-01-01T00:00:01Z")?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .run(|db| {
                let pending = pending_vectors(db, 64, "now")?;
                let entries = pending
                    .into_iter()
                    .enumerate()
                    .map(|(index, (memory_id, _, _))| VectorEntry {
                        memory_id,
                        model: "test-model".into(),
                        preprocess: TEXT_PREPROCESS.into(),
                        vector: if index == 0 {
                            vec![1.0, 0.0]
                        } else {
                            vec![0.0, 1.0]
                        },
                    })
                    .collect::<Vec<_>>();
                store_vectors(db, &entries, "test-model", "2026-01-01T00:00:01Z")?;
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fts_and_vectors_are_built_from_pending_and_owner_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        let first = seed(&store, "a", "柚子ジャムを作った").await;
        seed(&store, "b", "柚子ジャムはbの秘密").await;
        index(&store).await;
        let first_for_check = first.clone();
        store
            .run(move |db| {
                let result = search(
                    db,
                    "a",
                    "柚子ジャム",
                    Some(&[1.0, 0.0]),
                    Some("test-model"),
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert_eq!(result.fts, "ok");
                assert_eq!(result.memories.len(), 1);
                assert_eq!(result.memories[0].id, first_for_check);
                let recent = search(
                    db,
                    "a",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    Some("2025-01-01T00:00:00Z"),
                    None,
                    "now",
                )?;
                assert_eq!(recent.memories.len(), 1);
                let future = search(
                    db,
                    "a",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    Some("2027-01-01T00:00:00Z"),
                    None,
                    "now",
                )?;
                assert!(future.memories.is_empty());
                let other = search(
                    db,
                    "b",
                    "柚子ジャム",
                    Some(&[1.0, 0.0]),
                    Some("test-model"),
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert!(other.memories.iter().all(|memory| memory.owner == "b"));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn share_strips_private_sources_and_unshare_cannot_leak_from_a_stale_fts_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        let id = seed(&store, "a", "柚子ジャムを作った").await;
        index(&store).await;
        let id_for_check = id.clone();
        let _ = &id_for_check;
        store
            .run(move |db| {
                control(db, "a", &id, "share", "now")?;
                let shared = search(
                    db,
                    "b",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert_eq!(shared.memories.len(), 1);
                assert_eq!(shared.memories[0].owner, "a");
                assert_eq!(shared.memories[0].audience, "shared");
                assert!(
                    shared.memories[0]
                        .sources
                        .iter()
                        .all(|source| source.kind != SourceKind::Input)
                );
                control(db, "a", &id, "unshare", "now")?;
                // The FTS row is intentionally left in place; the canonical
                // audience check must still hide it.
                let fts_rows: i64 = db.query_row(
                    "SELECT COUNT(*) FROM mind_memories_fts WHERE memory_id=?1",
                    [&id],
                    |row| row.get(0),
                )?;
                assert_eq!(fts_rows, 1);
                let hidden = search(
                    db,
                    "b",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert!(hidden.memories.is_empty());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn forgetting_lowers_recall_without_deleting_and_does_not_revive_on_reindex() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        let id = seed(&store, "a", "柚子ジャムを作った").await;
        index(&store).await;
        store
            .run(move |db| {
                control(db, "a", &id, "forget", "now")?;
                let normalized = search(
                    db,
                    "a",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert!(normalized.memories.is_empty());
                let past = search(
                    db,
                    "a",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    true,
                    None,
                    None,
                    "now",
                )?;
                assert_eq!(past.memories.len(), 1);
                assert_eq!(past.memories[0].recall_preference, "forgotten");
                let count: i64 =
                    db.query_row("SELECT COUNT(*) FROM mind_memories", [], |row| row.get(0))?;
                assert_eq!(count, 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_forgotten_basis_does_not_become_a_normal_memory_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        let id = seed(&store, "a", "柚子ジャムを作った").await;
        store
            .run(move |db| {
                control(db, "a", &id, "forget", "now")?;
                let mut new = memory("柚子ジャムを作った日の記録");
                new.supersedes = vec![id.clone()];
                let outcome = consolidate(
                    db,
                    "a",
                    "forgotten-basis",
                    "hf",
                    &request(2, vec![new]),
                    &[source(SourceKind::Input, "input-f")],
                    10,
                    0,
                    METHOD_REVISION,
                    1,
                    10,
                    "now",
                )?;
                assert!(outcome.result.changed);
                assert!(outcome.advanced);
                let preference: String = db.query_row(
                    "SELECT recall_preference FROM mind_memories WHERE supersedes LIKE ?1",
                    [format!("%{id}%")],
                    |row| row.get(0),
                )?;
                assert_eq!(preference, "forgotten");
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_shared_memory_keeps_its_owner_correction_relation_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        store
            .run(|db| {
                let first = mind::update(
                    db,
                    "a",
                    "first",
                    "h1",
                    &request(1, vec![memory("相手は柚子が好き")]),
                    &[source(SourceKind::Input, "in1")],
                    "parent",
                    "now",
                )?
                .memories[0]
                    .id
                    .clone();
                let mut corrected = memory("相手はりんごが好き");
                corrected.supersedes = vec![first.clone()];
                let second = mind::update(
                    db,
                    "a",
                    "second",
                    "h2",
                    &request(2, vec![corrected]),
                    &[source(SourceKind::Input, "in2")],
                    "parent",
                    "now",
                )?
                .memories[0]
                    .id
                    .clone();
                control(db, "a", &first, "share", "now")?;
                let shared = search(db, "b", "柚子", None, None, 10, false, None, None, "now")?;
                let memory = shared
                    .memories
                    .iter()
                    .find(|memory| memory.id == first)
                    .expect("shared");
                assert_eq!(memory.superseded_by, vec![second.clone()]);
                assert!(read_visible(db, "b", &second)?.is_none());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_index_work_stops_retrying_until_configuration_resets_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("retry.db")).await.unwrap();
        let id = seed(&store, "a", "柚子ジャムを作った").await;
        store
            .run(move |db| {
                let pending = pending_vectors(db, 64, "now")?;
                assert_eq!(pending.len(), 1);
                mark_index_error(
                    db,
                    std::slice::from_ref(&id),
                    "vector",
                    "PermissionRequired",
                    true,
                    "now",
                )?;
                assert!(pending_vectors(db, 64, "now")?.is_empty());
                retry_index(db, "now")?;
                assert_eq!(pending_vectors(db, 64, "now")?.len(), 1);
                for _ in 0..4 {
                    mark_index_error(
                        db,
                        std::slice::from_ref(&id),
                        "vector",
                        "provider",
                        false,
                        "now",
                    )?;
                }
                assert!(pending_vectors(db, 64, "now")?.is_empty());
                retry_index(db, "now")?;
                assert_eq!(pending_vectors(db, 64, "now")?.len(), 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_terminal_skip_advances_without_a_model_job_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("skip.db");
        let store = Store::open(path.clone()).await.unwrap();
        store
            .run(|db| {
                skip_consolidation(db, "a", 7, 4, METHOD_REVISION, "now")?;
                let status = consolidation_status(db, "a", METHOD_REVISION, "now")?;
                assert_eq!((status.last_sequence, status.last_offset), (7, 4));
                assert_eq!(status.skipped, 1);
                assert_eq!(status.model_calls, 0);
                skip_consolidation(db, "a", 6, 0, METHOD_REVISION, "now")?;
                let status = consolidation_status(db, "a", METHOD_REVISION, "now")?;
                assert_eq!((status.last_sequence, status.last_offset), (7, 4));
                assert_eq!(status.skipped, 1);
                Ok(())
            })
            .await
            .unwrap();
        let reopened = Store::open(path).await.unwrap();
        reopened
            .run(|db| {
                let status = consolidation_status(db, "a", METHOD_REVISION, "now")?;
                assert_eq!((status.last_sequence, status.last_offset), (7, 4));
                assert_eq!(status.skipped, 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_consolidation_cursor_can_advance_inside_a_long_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        store
            .run(|db| {
                let outcome = consolidate(
                    db,
                    "a",
                    "partial-5",
                    "h5",
                    &request(1, Vec::new()),
                    &[source(SourceKind::Input, "part")],
                    1,
                    5,
                    METHOD_REVISION,
                    1,
                    5,
                    "now",
                )?;
                assert_eq!(outcome.status.last_offset, 5);
                let outcome = consolidate(
                    db,
                    "a",
                    "partial-10",
                    "h10",
                    &request(1, Vec::new()),
                    &[source(SourceKind::Input, "part")],
                    1,
                    10,
                    METHOD_REVISION,
                    1,
                    5,
                    "now",
                )?;
                assert_eq!(
                    (outcome.status.last_sequence, outcome.status.last_offset),
                    (1, 10)
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn consolidation_advances_the_cursor_in_the_same_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        store
            .run(|db| {
                // A parent's state-only mood change advances the mind revision
                // but not the memory revision; it must not invalidate the
                // pending consolidation batch.
                let mut mood = request(1, Vec::new());
                mood.state = Some(crate::domain::mind::StateUpdate {
                    emotion: Some(crate::domain::mind::Emotion {
                        description: "少し落ち着いている".into(),
                        target: None,
                        reason: None,
                        signals: Vec::new(),
                    }),
                    ..Default::default()
                });
                mind::update(
                    db,
                    "a",
                    "parent-mood",
                    "mood-hash",
                    &mood,
                    &[source(SourceKind::Input, "mood")],
                    "parent",
                    "now",
                )?;
                let request = request(1, vec![memory("一緒に柚子ジャムを作った")]);
                let sources = vec![
                    source(SourceKind::Input, "input-1"),
                    source(SourceKind::Reply, "reply-1"),
                ];
                let outcome = consolidate(
                    db,
                    "a",
                    "consolidate-a-0-10",
                    "hash",
                    &request,
                    &sources,
                    10,
                    0,
                    METHOD_REVISION,
                    1,
                    120,
                    "now",
                )?;
                assert!(outcome.result.changed);
                assert!(outcome.advanced);
                assert_eq!(outcome.status.last_sequence, 10);
                assert_eq!(outcome.status.jobs, 1);
                let again = consolidate(
                    db,
                    "a",
                    "consolidate-a-0-10",
                    "hash",
                    &request,
                    &sources,
                    10,
                    0,
                    METHOD_REVISION,
                    1,
                    120,
                    "now",
                )?;
                assert!(again.result.changed);
                assert!(!again.advanced);
                assert_eq!(again.status.jobs, 1);
                let count: i64 =
                    db.query_row("SELECT COUNT(*) FROM mind_memories", [], |row| row.get(0))?;
                assert_eq!(count, 1);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn consolidation_cannot_overwrite_current_mind_and_a_failure_keeps_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("m.db")).await.unwrap();
        store
            .run(|db| {
                let mut forbidden = request(1, Vec::new());
                forbidden.state = Some(crate::domain::mind::StateUpdate {
                    emotion: Some(crate::domain::mind::Emotion {
                        description: "上書き".into(),
                        target: None,
                        reason: None,
                        signals: Vec::new(),
                    }),
                    ..Default::default()
                });
                let denied = consolidate(
                    db,
                    "a",
                    "consolidate-a-0-5",
                    "hash",
                    &forbidden,
                    &[source(SourceKind::Input, "input-1")],
                    5,
                    0,
                    METHOD_REVISION,
                    1,
                    10,
                    "now",
                );
                assert!(matches!(denied, Err(Error::InvalidRequest)));
                let stale = consolidate(
                    db,
                    "a",
                    "consolidate-a-0-6",
                    "hash2",
                    &request(9, vec![memory("古い版")]),
                    &[source(SourceKind::Input, "input-2")],
                    6,
                    0,
                    METHOD_REVISION,
                    1,
                    10,
                    "now",
                );
                assert!(matches!(stale, Err(Error::RevisionConflict)));
                let status = consolidation_status(db, "a", METHOD_REVISION, "now")?;
                assert_eq!(status.last_sequence, 0);
                let count: i64 =
                    db.query_row("SELECT COUNT(*) FROM mind_memories", [], |row| row.get(0))?;
                assert_eq!(count, 0);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pending_index_work_survives_reopen_and_a_failed_index_keeps_the_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let store = Store::open(path.clone()).await.unwrap();
        seed(&store, "a", "柚子ジャムを作った").await;
        store.db.clone().close().await.unwrap();
        drop(store);
        let store = Store::open(path).await.unwrap();
        store
            .run(|db| {
                let fts = pending_fts(db, 64, "now")?;
                let vectors = pending_vectors(db, 64, "now")?;
                assert_eq!(fts.len(), 1);
                assert_eq!(vectors.len(), 1);
                // A failed index transaction rolls back only the index work; the
                // memory stays canonical and the pending row stays for a retry.
                db.execute_batch("DROP TABLE mind_memories_fts;")?;
                assert!(index_fts(db, &fts, "now").is_err());
                let count: i64 =
                    db.query_row("SELECT COUNT(*) FROM mind_memories", [], |row| row.get(0))?;
                assert_eq!(count, 1);
                let pending = pending_fts(db, 64, "now")?;
                assert_eq!(pending.len(), 1);
                let result = search(
                    db,
                    "a",
                    "柚子ジャム",
                    None,
                    None,
                    10,
                    false,
                    None,
                    None,
                    "now",
                )?;
                assert_eq!(result.memories.len(), 1);
                assert_eq!(result.fts, "fallback");
                Ok(())
            })
            .await
            .unwrap();
    }
}
