use crate::{
    model::{Model, truncate},
    store,
};
use anyhow::{Result, ensure};
use ene_core::mind::recall_rank;
use ene_rpc::Memory;
use sqlx::SqlitePool;
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

type Cache = HashMap<(String, String, i64), Arc<[f64]>>;

#[derive(Clone, Default)]
pub(crate) struct Vectors(Arc<parking_lot::Mutex<Cache>>);

impl Vectors {
    pub(crate) fn evict(&self, id: &str) {
        self.0.lock().retain(|(_, key, _), _| key != id);
    }

    pub(crate) async fn recall(
        &self,
        db: &SqlitePool,
        model: &Model,
        parameters: Recall<'_>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Memory>> {
        let Recall {
            companion,
            question,
            memories,
        } = parameters;
        let query = model
            .embed(vec![truncate(question, 1500).to_owned()], cancel)
            .await?;
        let query = query
            .first()
            .filter(|vector| !vector.is_empty() && vector.len() <= 8192)
            .ok_or_else(|| anyhow::anyhow!("invalid query embedding"))?;
        let dimensions = query.len();
        let space = store::fingerprint(&(
            &model.connection.id,
            &model.connection.endpoint,
            &model.connection.options_json,
            model.connection.revision,
            &model.selection.model,
            dimensions,
            "text-v1",
        ))?;
        let rows = sqlx::query_as::<_, (String, i64, String, u8, i64, String, bool)>(
            "SELECT id, revision, text, importance, last_used, companion_id, shared
             FROM memories WHERE (companion_id=? OR shared=1) AND importance>=2
             ORDER BY max(last_used,updated_at) DESC,id LIMIT 256",
        )
        .bind(companion)
        .fetch_all(db)
        .await?;
        let keys: Vec<_> = rows
            .iter()
            .map(|(id, revision, ..)| (space.clone(), id.clone(), *revision))
            .collect();
        let at_most = 32;
        let miss = {
            let cache = self.0.lock();
            keys.iter()
                .enumerate()
                .filter(|(_, key)| !cache.contains_key(*key))
                .map(|(index, _)| index)
                .take(at_most)
                .collect::<Vec<_>>()
        };
        if !miss.is_empty() {
            let input = miss
                .iter()
                .map(|&index| truncate(&rows[index].2, 1500).to_owned())
                .collect::<Vec<_>>();
            let vectors = model.embed(input, cancel).await?;
            ensure!(vectors.len() == miss.len(), "embedding batch size changed");
            let mut cache = self.0.lock();
            if cache.len() >= 4096 {
                cache.clear();
            }
            for (index, vector) in miss.into_iter().zip(vectors) {
                if vector.len() == dimensions && vector.iter().all(|value| value.is_finite()) {
                    cache.insert(keys[index].clone(), vector.into());
                }
            }
        }
        let mut semantic = {
            let cache = self.0.lock();
            keys.iter()
                .enumerate()
                .filter_map(|(index, key)| {
                    cache.get(key).map(|vector| (index, cosine(query, vector)))
                })
                .filter(|(_, score)| score.is_finite())
                .collect::<Vec<_>>()
        };
        semantic.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let lexical: Vec<_> = memories.iter().map(|memory| &memory.id).collect();
        let semantic: Vec<_> = semantic
            .iter()
            .take(64)
            .map(|(index, _)| &rows[*index].0)
            .collect();
        let ranks: HashMap<_, _> = recall_rank([lexical, semantic])
            .into_iter()
            .map(|(key, rank)| (key.clone(), rank))
            .collect();
        let mut candidates: Vec<_> = rows
            .into_iter()
            .filter_map(
                |(id, revision, text, importance, last_used, companion_id, shared)| {
                    let relevance = ranks.get(&id)?;
                    let score = ene_core::mind::recall_score(
                        *relevance,
                        importance,
                        last_used,
                        crate::actor::now(),
                    );
                    Some((
                        Memory {
                            id,
                            revision,
                            text,
                            importance: importance.into(),
                            companion_id,
                            shared,
                            ..Default::default()
                        },
                        score,
                    ))
                },
            )
            .collect();
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
        let mut output: Vec<_> = memories
            .iter()
            .take_while(|memory| memory.importance == 10)
            .cloned()
            .collect();
        for (memory, _) in candidates {
            if output.len() >= 12 {
                break;
            }
            if !output.iter().any(|saved| saved.id == memory.id) {
                output.push(memory);
            }
        }
        Ok(output)
    }
}

pub(crate) struct Recall<'a> {
    pub companion: &'a str,
    pub question: &'a str,
    pub memories: &'a [Memory],
}

fn cosine(query: &[f64], vector: &[f64]) -> f64 {
    let (dot, a, b) = query
        .iter()
        .zip(vector)
        .fold((0.0, 0.0, 0.0), |(dot, a, b), (x, y)| {
            (dot + x * y, a + x * x, b + y * y)
        });
    dot / (a * b).sqrt()
}
