//! Canonical episodic memory table and SQLite FTS5 trigram lexical search.
use super::{Database, Result, Tx, storage};
use crate::host::mind::{FORGOTTEN, MemoryRecord, SHARED};
use rusqlite::{Connection, OptionalExtension, params};

impl Tx<'_> {
    pub(crate) fn put_memory(&self, memory: &MemoryRecord) -> Result<()> {
        let value = serde_json::to_string(memory).map_err(storage)?;
        self.0
            .execute(
                "INSERT INTO memories(id, owner, audience, recall_preference, recorded_at, value) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(id) DO UPDATE SET \
                   owner=excluded.owner, \
                   audience=excluded.audience, \
                   recall_preference=excluded.recall_preference, \
                   recorded_at=excluded.recorded_at, \
                   value=excluded.value",
                params![
                    memory.id,
                    memory.owner,
                    memory.audience,
                    memory.recall_preference,
                    memory.recorded_at,
                    value
                ],
            )
            .map_err(storage)?;
        self.0
            .execute("DELETE FROM memories_fts WHERE id=?1", [&memory.id])
            .map_err(storage)?;
        self.0
            .execute(
                "INSERT INTO memories_fts(id, content, description, appraisal) VALUES (?1, ?2, ?3, ?4)",
                params![memory.id, memory.content, memory.description, memory.appraisal],
            )
            .map_err(storage)?;
        Ok(())
    }

    pub(crate) fn get_memory(&self, id: &str) -> Result<Option<MemoryRecord>> {
        read_memory(&self.0, id)
    }
}

fn read_memory(db: &Connection, id: &str) -> Result<Option<MemoryRecord>> {
    let value: Option<String> = db
        .query_row("SELECT value FROM memories WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(storage)?;
    value
        .map(|json| serde_json::from_str(&json).map_err(storage))
        .transpose()
}

impl Database {
    pub(crate) fn get_memory(&self, id: &str) -> Result<Option<MemoryRecord>> {
        let db = self.lock()?;
        read_memory(&db, id)
    }

    pub(crate) fn count_memories(&self, owner: &str) -> Result<u64> {
        let db = self.lock()?;
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM memories WHERE owner=?1",
                [owner],
                |row| row.get(0),
            )
            .map_err(storage)?;
        Ok(count.max(0) as u64)
    }

    pub(crate) fn search_memories(
        &self,
        viewer: &str,
        query: &str,
        include_forgotten: bool,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let db = self.lock()?;
        let direct = format!("companion:{viewer}");
        let forgotten = i32::from(include_forgotten);
        let max = limit.min(64) as i64;
        let Some(fts_query) = fts_query(query) else {
            // Shorter than one trigram: the index cannot answer, so the
            // memories are scanned. Recalls run on every input, so this is
            // not reached for a sentence, only for a couple of characters.
            return query_memories(
                &db,
                SCAN,
                params![viewer, SHARED, direct, forgotten, FORGOTTEN, query, max],
            );
        };
        query_memories(
            &db,
            MATCH,
            params![fts_query, viewer, SHARED, direct, forgotten, FORGOTTEN, max],
        )
    }
}

const MATCH: &str = "SELECT m.value FROM memories_fts \
     JOIN memories m ON m.id = memories_fts.id \
     WHERE memories_fts MATCH ?1 \
       AND (m.owner = ?2 OR m.audience = ?3 OR m.audience = ?4) \
       AND (?5 != 0 OR m.recall_preference != ?6) \
     ORDER BY bm25(memories_fts), m.recorded_at DESC \
     LIMIT ?7";

const SCAN: &str = "SELECT m.value FROM memories m \
     JOIN memories_fts f ON f.id = m.id \
     WHERE (m.owner = ?1 OR m.audience = ?2 OR m.audience = ?3) \
       AND (?4 != 0 OR m.recall_preference != ?5) \
       AND ( \
         instr(f.content, ?6) > 0 OR instr(?6, f.content) > 0 OR \
         (f.description != '' AND (instr(f.description, ?6) > 0 OR instr(?6, f.description) > 0)) OR \
         (f.appraisal != '' AND (instr(f.appraisal, ?6) > 0 OR instr(?6, f.appraisal) > 0)) \
       ) \
     ORDER BY m.recorded_at DESC \
     LIMIT ?7";

/// How many trigrams of one message may go into a query: recall runs on
/// every input, so a long message must not build an unbounded MATCH.
const MAX_TERMS: usize = 32;

/// The FTS5 query for `query`, or `None` when nothing in it can reach the
/// trigram index.
///
/// The table tokenizes trigrams, and a quoted string matches as a substring.
/// One phrase per word finds an English word; a Japanese sentence has no
/// spaces and is never a substring of a memory, so a longer word is cut into
/// overlapping three-character windows that any one of may match, and bm25
/// puts the memories that share more of them first.
fn fts_query(query: &str) -> Option<String> {
    let mut terms = Vec::new();
    for word in query.split_whitespace() {
        let chars: Vec<char> = word.chars().collect();
        if chars.len() < 3 {
            continue;
        }
        if chars.len() == 3 {
            terms.push(phrase(word));
        } else {
            terms.extend(
                chars
                    .windows(3)
                    .take(MAX_TERMS.saturating_sub(terms.len()))
                    .map(|window| phrase(&window.iter().collect::<String>())),
            );
        }
        if terms.len() >= MAX_TERMS {
            break;
        }
    }
    terms.truncate(MAX_TERMS);
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

fn phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

fn query_memories(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<MemoryRecord>> {
    let mut statement = db.prepare(sql).map_err(storage)?;
    statement
        .query_map(params, |row| row.get::<_, String>(0))
        .map_err(storage)?
        .map(|row| serde_json::from_str(&row.map_err(storage)?).map_err(storage))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::mind::NORMAL;

    #[test]
    fn memories_and_fts_support_trigram_search_audience_and_forgetting() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(&temp.path().join("test.sqlite3")).unwrap();
        let memory = MemoryRecord {
            id: "mem-1".into(),
            owner: "c1".into(),
            kind: "experience".into(),
            audience: "private".into(),
            content: "青森の美味しいりんごを食べた".into(),
            description: "りんごの記憶".into(),
            appraisal: "とても美味しかった".into(),
            acquisition: "direct_experience".into(),
            sources: vec![],
            recorded_at: "2026-11-10T00:00:00Z".into(),
            occurred_at: None,
            supersedes: vec![],
            recall_preference: NORMAL.into(),
            created_by: "parent".into(),
            revision: 1,
            updated_at: "2026-11-10T00:00:00Z".into(),
        };
        db.transaction(|tx| tx.put_memory(&memory)).unwrap();

        // 1. Trigram search for "美味しいりんご" (>=3 chars)
        let found = db
            .search_memories("c1", "美味しいりんご", false, 10)
            .unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "mem-1");

        // 2. Short 2-char fallback search for "青森" (<3 chars)
        let found_short = db.search_memories("c1", "青森", false, 10).unwrap();
        assert_eq!(found_short.len(), 1);
        assert_eq!(found_short[0].id, "mem-1");

        // 3. A Japanese sentence that is not a substring of the memory still
        //    finds it: the query is cut into trigrams, and bm25 ranks them.
        let sentence = db
            .search_memories("c1", "昨日話した青森のりんごの話、覚えてる？", false, 10)
            .unwrap();
        assert_eq!(sentence.len(), 1);
        assert_eq!(sentence[0].id, "mem-1");
        assert!(
            db.search_memories("c1", "今日は天気がいいですね", false, 10)
                .unwrap()
                .is_empty(),
            "an unrelated sentence must not recall anything"
        );

        // 4. Privacy boundary: c2 cannot see c1's private memory
        let other = db.search_memories("c2", "りんご", false, 10).unwrap();
        assert!(other.is_empty());

        // 5. Forgetting: forgotten memory excluded unless include_forgotten is true
        let mut forgotten = memory;
        forgotten.recall_preference = FORGOTTEN.into();
        db.transaction(|tx| tx.put_memory(&forgotten)).unwrap();

        let hidden = db.search_memories("c1", "りんご", false, 10).unwrap();
        assert!(hidden.is_empty());
        let shown = db.search_memories("c1", "りんご", true, 10).unwrap();
        assert_eq!(shown.len(), 1);
    }
}
