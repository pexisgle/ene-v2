use crate::{
    actor::{Actor, id, now},
    configuration::save,
    error::{conflict, invalid, missing},
    store,
};
use anyhow::Result;
use ene_core::mind::{Fact, Reflection, Summary, recall_rank, recall_score};
use ene_rpc::{self as w, memory_change::Operation};
use sqlx::{FromRow, SqliteConnection};
use std::collections::HashMap;

#[derive(Clone, FromRow)]
pub(crate) struct MemoryRow {
    id: String,
    companion_id: String,
    revision: i64,
    shared: bool,
    text: String,
    importance: u8,
    provenance: String,
    superseded_by: Option<String>,
    last_used: i64,
}

impl From<MemoryRow> for w::Memory {
    fn from(row: MemoryRow) -> Self {
        Self {
            id: row.id,
            companion_id: row.companion_id,
            revision: row.revision,
            shared: row.shared,
            text: row.text,
            importance: row.importance.into(),
            provenance: row.provenance,
            superseded_by: row.superseded_by.unwrap_or_default(),
            ..Default::default()
        }
    }
}

async fn search(
    db: &mut SqliteConnection,
    companion: &str,
    query: &str,
    minimum: u8,
    limit: u32,
) -> Result<Vec<MemoryRow>> {
    let query = query.trim();
    let words: Vec<_> = query
        .split_whitespace()
        .filter(|word| word.chars().count() >= 3)
        .take(12)
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    if !words.is_empty() {
        return Ok(sqlx::query_as(
            "SELECT m.id,m.companion_id,m.revision,m.shared,m.text,m.importance,m.provenance,m.superseded_by,m.last_used FROM memory_fts JOIN memories m ON m.rowid=memory_fts.rowid
             WHERE memory_fts MATCH ? AND (m.companion_id=? OR m.shared=1) AND importance>=?
             ORDER BY rank,m.id LIMIT ?",
        )
        .bind(words.join(" OR ")).bind(companion).bind(minimum).bind(limit)
        .fetch_all(db).await?);
    }
    Ok(sqlx::query_as(
        "SELECT m.id,m.companion_id,m.revision,m.shared,m.text,m.importance,m.provenance,m.superseded_by,m.last_used FROM memories m WHERE (companion_id=? OR shared=1) AND importance>=?
         AND instr(lower(text), lower(?))>0 ORDER BY importance DESC,last_used DESC,id LIMIT ?",
    )
    .bind(companion).bind(minimum).bind(query).bind(limit)
    .fetch_all(db).await?)
}

pub(crate) trait MemoryCommands {
    async fn mind(&self, companion: &str) -> Result<w::Mind>;
    async fn put_mind(&mut self, request: w::MindChange) -> Result<w::Mind>;
    async fn search_memories(&self, request: w::MemoryQuery) -> Result<w::Memories>;
    async fn recall(&self, companion: &str, query: &str) -> Result<Vec<w::Memory>>;
    async fn visible_memories(
        &self,
        companion: &str,
        candidates: Vec<w::Memory>,
    ) -> Result<Vec<w::Memory>>;
    async fn change_memory(&mut self, request: w::MemoryChange) -> Result<w::Memory>;
    async fn commit_reflection(
        &mut self,
        companion: &str,
        progress: ReflectionProgress,
    ) -> Result<()>;
    async fn restore_summary(&mut self, companion: &str) -> Result<()>;
}

impl MemoryCommands for Actor {
    async fn mind(&self, companion: &str) -> Result<w::Mind> {
        self.companion(companion).await?;
        let mut db = self.db.acquire().await?;
        Ok(store::get(&mut db, "mind", companion)
            .await?
            .unwrap_or_default())
    }

    async fn put_mind(&mut self, request: w::MindChange) -> Result<w::Mind> {
        self.companion(&request.companion_id).await?;
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_mind", &request))?;
        let mut mind = request
            .mind
            .into_option()
            .ok_or_else(|| invalid("state required"))?;
        if serde_json::to_vec(&mind)?.len() > 8000 {
            return Err(invalid("mind state exceeds size limit"));
        }
        mind.revision = key.revision + 1;
        mind.observed_at = now();
        save(&self.db, "mind", key, &request.companion_id, &hash, mind).await
    }

    async fn search_memories(&self, request: w::MemoryQuery) -> Result<w::Memories> {
        self.companion(&request.companion_id).await?;
        let mut db = self.db.acquire().await?;
        let rows = search(
            &mut db,
            &request.companion_id,
            &request.query,
            0,
            request.limit.clamp(1, 100),
        )
        .await?;
        Ok(w::Memories {
            items: rows.into_iter().map(Into::into).collect(),
            ..Default::default()
        })
    }

    async fn recall(&self, companion: &str, query: &str) -> Result<Vec<w::Memory>> {
        let mut db = self.db.acquire().await?;
        let pinned: Vec<MemoryRow> = sqlx::query_as(
            "SELECT m.id,m.companion_id,m.revision,m.shared,m.text,m.importance,m.provenance,m.superseded_by,m.last_used FROM memories m WHERE (companion_id=? OR shared=1) AND importance=10
             ORDER BY max(last_used,updated_at) DESC,id LIMIT 64",
        ).bind(companion).fetch_all(&mut *db).await?;
        let mut remaining = 2000usize;
        let mut selected = HashMap::new();
        for row in pinned {
            if let Some(left) = remaining.checked_sub(row.text.len()) {
                remaining = left;
                selected.insert(row.id.clone(), row);
            }
        }
        let lexical = search(&mut db, companion, query, 2, 50).await?;
        let keys: Vec<_> = lexical.iter().map(|row| &row.id).collect();
        let ranks = recall_rank([keys, Vec::new()]);
        let time = now();
        let mut relevant = lexical
            .iter()
            .filter(|row| !selected.contains_key(&row.id))
            .collect::<Vec<_>>();
        relevant.sort_by(|a, b| {
            let score =
                |row: &MemoryRow| recall_score(ranks[&row.id], row.importance, row.last_used, time);
            score(b).total_cmp(&score(a))
        });
        let mut pinned: Vec<_> = selected.into_values().collect();
        pinned.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(a.id.cmp(&b.id)));
        Ok(pinned
            .into_iter()
            .chain(relevant.into_iter().take(6).cloned())
            .map(Into::into)
            .collect())
    }

    async fn visible_memories(
        &self,
        companion: &str,
        mut candidates: Vec<w::Memory>,
    ) -> Result<Vec<w::Memory>> {
        let mut current = Vec::new();
        for memory in candidates.drain(..) {
            if sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM memories WHERE id=? AND revision=? AND importance>=2 AND (companion_id=? OR shared=1))",
            ).bind(&memory.id).bind(memory.revision).bind(companion).fetch_one(&self.db).await? {
                current.push(memory);
            }
        }
        Ok(current)
    }

    async fn change_memory(&mut self, request: w::MemoryChange) -> Result<w::Memory> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("change_memory", &request))?;
        let mut value = request
            .memory
            .into_option()
            .ok_or_else(|| invalid("memory required"))?;
        let operation = request
            .operation
            .as_known()
            .ok_or_else(|| invalid("invalid memory operation"))?;
        self.companion(&value.companion_id).await?;
        let mut tx = self.db.begin().await?;
        if let Some(saved) = store::replay(&mut tx, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        if operation == Operation::Remember {
            validate_fact(&value.text, value.importance)?;
            value.id = id();
            value.revision = 1;
            insert(&mut tx, &value).await?;
        } else {
            let previous = sqlx::query_as::<_, MemoryRow>(
                "SELECT m.id,m.companion_id,m.revision,m.shared,m.text,m.importance,m.provenance,m.superseded_by,m.last_used FROM memories m WHERE id=? AND (companion_id=? OR shared=1)"
            ).bind(&value.id).bind(&value.companion_id).fetch_optional(&mut *tx).await?
                .ok_or_else(|| missing("memory not found"))?;
            if previous.revision != key.revision {
                return Err(conflict("memory changed"));
            }
            value = change(&mut tx, operation, previous.into(), &value).await?;
        }
        store::acknowledge(&mut tx, &key.request_key, &hash, &value).await?;
        tx.commit().await?;
        self.models.vectors.evict(&value.id);
        if operation == Operation::Erase {
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&self.db)
                .await?;
        }
        Ok(value)
    }

    async fn commit_reflection(
        &mut self,
        companion: &str,
        progress: ReflectionProgress,
    ) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let current: w::Mind = store::get(&mut tx, "mind", companion)
            .await?
            .unwrap_or_default();
        if current.revision != progress.mind_revision {
            return Err(conflict("mind changed during reflection"));
        }
        let reflection = progress.reflection;
        if reflection.summary.trim().is_empty() || reflection.summary.len() > 8000 {
            return Err(invalid("invalid summary"));
        }
        let state = reflection.mind;
        let mind = w::Mind {
            revision: current.revision + 1,
            emotion: state.emotion,
            relationship: state.relationship,
            tendencies: state.tendencies,
            interests: state.interests,
            observed_at: now(),
            ..Default::default()
        };
        store::put(&mut tx, "mind", companion, current.revision, &mind).await?;
        for fact in reflection.facts.into_iter().take(30) {
            add_reflected_fact(&mut tx, companion, fact, &progress.provenance).await?;
        }
        for key in reflection.cited_memories {
            if let Some(&revision) = progress.visible.get(&key) {
                sqlx::query("UPDATE memories SET last_used=unixepoch() WHERE id=? AND revision=? AND importance>=2 AND (companion_id=? OR shared=1)")
                    .bind(&key).bind(revision).bind(companion).execute(&mut *tx).await?;
            }
        }
        let through = i64::try_from(progress.through)?;
        sqlx::query(
            "INSERT INTO reflection_checkpoints VALUES(?,?,?)
             ON CONFLICT(companion_id) DO UPDATE SET through=excluded.through,summary=excluded.summary
             WHERE excluded.through > reflection_checkpoints.through",
        ).bind(companion).bind(through).bind(&reflection.summary).execute(&mut *tx).await?;
        tx.commit().await?;
        self.restore_summary(companion).await
    }

    async fn restore_summary(&mut self, companion: &str) -> Result<()> {
        let checkpoint = sqlx::query_as::<_, (i64, String)>(
            "SELECT through,summary FROM reflection_checkpoints WHERE companion_id=?",
        )
        .bind(companion)
        .fetch_optional(&self.db)
        .await?;
        if let Some((through, text)) = checkpoint {
            let journal = self.journal(companion).await?;
            let through = u64::try_from(through)?;
            if journal
                .summary
                .as_ref()
                .is_none_or(|summary| through > summary.through)
            {
                journal
                    .append(ene_core::mind::Record::Summary(Summary { through, text }))
                    .await?;
            }
        }
        Ok(())
    }
}

pub(crate) struct ReflectionProgress {
    pub through: u64,
    pub mind_revision: i64,
    pub reflection: Reflection,
    pub visible: HashMap<String, i64>,
    pub provenance: String,
}

fn validate_fact(text: &str, importance: u32) -> Result<()> {
    if text.trim().is_empty() || text.len() > 2000 || importance > 10 {
        return Err(invalid(
            "memory must contain one fact and importance from 0 to 10",
        ));
    }
    Ok(())
}

async fn insert(db: &mut SqliteConnection, memory: &w::Memory) -> Result<()> {
    sqlx::query("INSERT INTO memories(id,companion_id,shared,revision,text,importance,provenance,updated_at,last_used) VALUES(?,?,?,?,?,?,?,unixepoch(),unixepoch())")
        .bind(&memory.id).bind(&memory.companion_id).bind(memory.shared).bind(memory.revision)
        .bind(&memory.text).bind(memory.importance).bind(&memory.provenance).execute(db).await?;
    Ok(())
}

async fn add_reflected_fact(
    db: &mut SqliteConnection,
    companion: &str,
    fact: Fact,
    provenance: &str,
) -> Result<()> {
    let text = fact.text.trim();
    if validate_fact(text, fact.importance.into()).is_err() || fact.importance < 2 {
        return Ok(());
    }
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM memories WHERE (companion_id=? OR shared=1) AND lower(trim(text))=lower(?))",
    ).bind(companion).bind(text).fetch_one(&mut *db).await?;
    if !exists {
        insert(
            db,
            &w::Memory {
                id: id(),
                companion_id: companion.into(),
                shared: fact.shared,
                revision: 1,
                text: text.into(),
                importance: fact.importance.into(),
                provenance: provenance.into(),
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}

async fn change(
    db: &mut SqliteConnection,
    operation: Operation,
    mut old: w::Memory,
    new: &w::Memory,
) -> Result<w::Memory> {
    match operation {
        Operation::Correct => {
            validate_fact(&new.text, new.importance)?;
            let replacement = w::Memory {
                id: id(),
                revision: 1,
                companion_id: old.companion_id.clone(),
                shared: old.shared,
                text: new.text.clone(),
                importance: new.importance,
                provenance: new.provenance.clone(),
                ..Default::default()
            };
            insert(&mut *db, &replacement).await?;
            sqlx::query("UPDATE memories SET importance=min(importance,2),superseded_by=?,revision=revision+1,updated_at=unixepoch() WHERE id=?")
                .bind(&replacement.id).bind(&old.id).execute(db).await?;
            return Ok(replacement);
        }
        Operation::Forget => old.importance = 0,
        Operation::Share => {
            old.shared = new.shared;
            if !old.shared {
                old.companion_id.clone_from(&new.companion_id);
            }
        }
        Operation::Erase => {
            sqlx::query("DELETE FROM memories WHERE id=?")
                .bind(&old.id)
                .execute(&mut *db)
                .await?;
            sqlx::query("DELETE FROM receipts WHERE json_extract(response,'$.id')=?")
                .bind(&old.id)
                .execute(db)
                .await?;
            return Ok(w::Memory {
                id: old.id,
                ..Default::default()
            });
        }
        _ => return Err(invalid("invalid memory operation")),
    }
    old.revision += 1;
    sqlx::query("UPDATE memories SET importance=?,shared=?,companion_id=?,revision=?,updated_at=unixepoch(),last_used=unixepoch() WHERE id=?")
        .bind(old.importance).bind(old.shared).bind(&old.companion_id).bind(old.revision).bind(&old.id).execute(db).await?;
    Ok(old)
}
