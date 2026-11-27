use super::{FIELDS, VISIBLE};
use crate::{error::Result, row::FromRow, to_json};
use ene_mind::{Memory, RecallCandidate, RecallMode, recall_terms};
use itertools::Itertools as _;
use rusqlite::{Connection, params};

const CANDIDATES: usize = 20;
const ELIGIBLE: &str = "m.importance >= ?2 AND m.id NOT IN (SELECT value FROM json_each(?4))";

pub(super) fn lexical_candidates(
    connection: &Connection,
    companion: &str,
    query: &str,
    mode: RecallMode<'_>,
) -> Result<Vec<Memory>> {
    let (long, short): (Vec<_>, Vec<_>) = recall_terms(query)
        .unique()
        .partition(|term| term.chars().nth(2).is_some());
    if long.is_empty() && short.is_empty() {
        return Ok(Vec::new());
    }
    let excluded = mode.exclusions().map(to_json).transpose()?;
    let read = |sql: &str, terms: &str| -> rusqlite::Result<Vec<Memory>> {
        connection
            .prepare_cached(sql)?
            .query_map(
                params![companion, mode.minimum_importance(), terms, excluded],
                Memory::from_row,
            )?
            .collect()
    };
    let full_text = if long.is_empty() {
        Vec::new()
    } else {
        let phrases = long
            .into_iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .join(" OR ");
        read(
            &format!(
                "SELECT m.* FROM memories_fts f JOIN memories m ON m.rowid = f.rowid \
                 WHERE {VISIBLE} AND {ELIGIBLE} AND memories_fts MATCH ?3 \
                 ORDER BY bm25(memories_fts), f.rowid LIMIT {CANDIDATES}"
            ),
            &phrases,
        )?
    };
    if short.is_empty() {
        return Ok(full_text);
    }
    let (sql, patterns) = short_query(&short)?;
    let partial = read(&sql, &patterns)?;
    if full_text.is_empty() {
        return Ok(partial);
    }
    Ok(full_text
        .into_iter()
        .interleave(partial)
        .unique_by(|memory| memory.id.clone())
        .take(CANDIDATES)
        .collect())
}

pub(super) fn semantic_memories(
    connection: &Connection,
    companion: &str,
    mode: RecallMode<'_>,
    candidates: &[RecallCandidate],
) -> Result<Vec<Memory>> {
    let candidates = candidates
        .iter()
        .filter(|item| {
            !item.id.is_empty()
                && !mode
                    .exclusions()
                    .is_some_and(|ids| ids.contains(item.id.as_str()))
        })
        .unique()
        .take(64)
        .collect_vec();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(&format!(
        "SELECT m.* FROM json_each(?3) AS candidate CROSS JOIN memories m \
         WHERE m.id = candidate.value ->> '$.id' \
         AND m.content_revision = candidate.value ->> '$.content_revision' \
         AND {VISIBLE} AND m.importance >= ?2 ORDER BY candidate.key"
    ))?;
    let rows = statement.query_map(
        params![companion, mode.minimum_importance(), to_json(&candidates)?],
        Memory::from_row,
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

fn short_query(terms: &[&str]) -> Result<(String, String)> {
    let pattern = |term: &str| {
        format!(
            "%{}%",
            term.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        )
    };
    let ordered = |scope, predicate| {
        format!(
            "SELECT m.rowid, {FIELDS} FROM memories m WHERE {scope} AND {ELIGIBLE} AND {predicate} \
             ORDER BY m.importance DESC, m.rowid DESC LIMIT {CANDIDATES}"
        )
    };
    if let [term] = terms {
        return Ok((
            ordered(VISIBLE, "m.content LIKE ?3 ESCAPE '\\'"),
            pattern(term),
        ));
    }
    let predicate =
        "EXISTS (SELECT 1 FROM json_each(?3) term WHERE m.content LIKE term.value ESCAPE '\\')";
    let private = ordered("m.scope = ?1", predicate);
    let shared = ordered("m.scope = 'shared' AND ?1 <> 'shared'", predicate);
    // An entry below the top candidates in its scope cannot reach the combined top list.
    let sql = format!(
        "SELECT * FROM ({private}) UNION ALL SELECT * FROM ({shared}) \
         ORDER BY importance DESC, rowid DESC LIMIT {CANDIDATES}"
    );
    Ok((
        sql,
        to_json(&terms.iter().map(|term| pattern(term)).collect_vec())?,
    ))
}
