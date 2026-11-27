//! In-memory compact vector index mirroring SQLite `memory_embeddings`
//! per `(companion, space)` so top-k ranking is isolated by companion and space.
use super::dimensions_of_space;
use crate::host::{
    db::{Result, Store},
    error::HostError,
};
use bevy_ecs::prelude::*;
use ene_mind::{RecallCandidate, RecallMode};
use itertools::Itertools;
use std::collections::HashMap;

/// The f32 LE format stored in `memory_embeddings.vector`.
pub(super) fn encode(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(HostError::Storage(("stored embedding is corrupt").into()));
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    Ok(chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect())
}

/// One embedding space as one contiguous `Vec<f32>`: `entries[i]` is the
/// memory and revision vector `i` belongs to, stored at `i * dimensions`.
#[derive(Default)]
pub(super) struct Space {
    dimensions: usize,
    entries: Vec<(RecallCandidate, u8)>,
    vectors: Vec<f32>,
    magnitudes: Vec<f64>,
}

impl Space {
    fn refresh_importance(&mut self, rows: &[(RecallCandidate, u8)]) -> bool {
        if self.entries == rows {
            return true;
        }
        if self.entries.len() != rows.len()
            || self
                .entries
                .iter()
                .zip(rows)
                .any(|((cached, _), (current, _))| cached != current)
        {
            return false;
        }
        for ((_, importance), (_, current)) in self.entries.iter_mut().zip(rows) {
            *importance = *current;
        }
        true
    }

    pub(super) fn push(
        &mut self,
        candidate: RecallCandidate,
        importance: u8,
        vector: &[f32],
        magnitude: f64,
    ) {
        if self.dimensions != 0 && vector.len() != self.dimensions {
            return;
        }
        self.dimensions = vector.len();
        self.entries.push((candidate, importance));
        self.vectors.extend_from_slice(vector);
        self.magnitudes.push(magnitude);
    }

    fn vector(&self, index: usize) -> Option<&[f32]> {
        if self.dimensions == 0 {
            return None;
        }
        self.vectors
            .get(index * self.dimensions..(index + 1) * self.dimensions)
    }
}

/// The ECS cache of the canonical vectors. Each `(companion, space)` keeps one
/// flat `Vec<f32>`, so another companion's vectors can never take the top-k
/// slots of that companion's ranking; `ranked` keeps only the best `limit`
/// without sorting or allocating for the rest.
#[derive(Resource, Default)]
pub(crate) struct CompactVectorIndex {
    pub(super) spaces: HashMap<(String, String), Space>,
}

impl CompactVectorIndex {
    /// Mirrors one companion's space from SQLite. Vectors whose content
    /// revision still matches are reused, so an unchanged space reads no BLOB
    /// column.
    pub(crate) fn sync(&mut self, store: &Store, companion: &str, space: &str) -> Result<()> {
        let Some(dimensions) = dimensions_of_space(space) else {
            return Ok(());
        };
        let rows = store.memory_repo().embedding_revisions(space, companion)?;
        let key = (companion.to_owned(), space.to_owned());
        if self
            .spaces
            .get_mut(&key)
            .is_some_and(|current| current.refresh_importance(&rows))
        {
            return Ok(());
        }
        let previous = self.spaces.remove(&key);
        let mut reusable = HashMap::new();
        if let Some(previous) = &previous {
            for (index, (candidate, _)) in previous.entries.iter().enumerate() {
                if let Some(vector) = previous.vector(index) {
                    reusable.insert(candidate, (vector, previous.magnitudes[index]));
                }
            }
        }
        let mut next = Space {
            dimensions,
            ..Space::default()
        };
        for (candidate, importance) in rows {
            if let Some(&(vector, magnitude)) = reusable.get(&candidate) {
                next.push(candidate, importance, vector, magnitude);
                continue;
            }
            let Some(bytes) = store.memory_repo().embedding_blob(&candidate, space)? else {
                continue;
            };
            let vector = decode(&bytes)?;
            if vector.len() != dimensions {
                return Err(HostError::Storage(
                    ("stored embedding dimensions do not match its length").into(),
                ));
            }
            let magnitude = dot(&vector, &vector).sqrt();
            next.push(candidate, importance, &vector, magnitude);
        }
        self.spaces.insert(key, next);
        Ok(())
    }

    /// The cosine ranking of one companion's space, highest first. Only the
    /// best `limit` candidates are kept, so the scan neither sorts the whole
    /// space nor allocates an entry per memory.
    pub(crate) fn ranked(
        &self,
        companion: &str,
        space: &str,
        query: &[f32],
        mode: RecallMode<'_>,
        limit: usize,
    ) -> Vec<RecallCandidate> {
        let Some(current) = self.spaces.get(&(companion.to_owned(), space.to_owned())) else {
            return Vec::new();
        };
        if query.len() != current.dimensions || limit == 0 {
            return Vec::new();
        }
        let magnitude = dot(query, query).sqrt();
        if magnitude == 0.0 || !magnitude.is_finite() {
            return Vec::new();
        }
        current
            .entries
            .iter()
            .enumerate()
            .filter(|(_, (candidate, importance))| {
                *importance >= mode.minimum_importance()
                    && !mode
                        .exclusions()
                        .is_some_and(|ids| ids.contains(candidate.id.as_str()))
            })
            .filter_map(|(index, _)| {
                let score =
                    dot(query, current.vector(index)?) / (magnitude * current.magnitudes[index]);
                score.is_finite().then_some((index, score.clamp(-1.0, 1.0)))
            })
            .k_smallest_by(limit, |(left, left_score), (right, right_score)| {
                right_score
                    .total_cmp(left_score)
                    .then_with(|| left.cmp(right))
            })
            .map(|(index, _)| current.entries[index].0.clone())
            .collect()
    }
}

pub(super) fn dot(a: &[f32], b: &[f32]) -> f64 {
    // Match Rig's f64 reduction order without widening the stored vectors.
    a.chunks(256)
        .zip(b.chunks(256))
        .map(|(a, b)| {
            a.iter()
                .zip(b)
                .map(|(&x, &y)| f64::from(x) * f64::from(y))
                .sum::<f64>()
        })
        .sum()
}
