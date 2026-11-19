//! In-memory compact vector index mirroring SQLite `memories.embedding`
//! per `(companion, space)` so top-k ranking is isolated by companion and space.
use super::dimensions_of_space;
use crate::host::{
    db::{Result, Store},
    error::HostError,
};
use bevy_ecs::prelude::*;
use itertools::Itertools;
use std::collections::HashMap;

/// The f32 LE wire format stored in `memories.embedding`.
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
    entries: Vec<(String, u64)>,
    vectors: Vec<f32>,
    by_id: HashMap<String, usize>,
}

impl Space {
    fn matches(&self, rows: &[(String, u64)]) -> bool {
        self.entries.as_slice() == rows
    }

    pub(super) fn push(&mut self, id: &str, revision: u64, vector: &[f32]) -> bool {
        if self.dimensions != 0 && vector.len() != self.dimensions {
            return false;
        }
        self.dimensions = vector.len();
        self.by_id.insert(id.to_owned(), self.entries.len());
        self.entries.push((id.to_owned(), revision));
        self.vectors.extend_from_slice(vector);
        true
    }

    fn vector(&self, index: usize) -> Option<&[f32]> {
        if self.dimensions == 0 {
            return None;
        }
        self.vectors
            .get(index * self.dimensions..(index + 1) * self.dimensions)
    }

    fn vector_for(&self, id: &str, revision: u64) -> Option<&[f32]> {
        let index = *self.by_id.get(id)?;
        (self.entries.get(index).map(|entry| entry.1) == Some(revision))
            .then(|| self.vector(index))
            .flatten()
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
        let rows = store.embedding_revisions(space, companion)?;
        let key = (companion.to_owned(), space.to_owned());
        if self
            .spaces
            .get(&key)
            .is_some_and(|current| current.matches(&rows))
        {
            return Ok(());
        }
        let previous = self.spaces.remove(&key);
        let mut next = Space {
            dimensions: previous
                .as_ref()
                .map(|current| current.dimensions)
                .filter(|dimensions| *dimensions > 0)
                .or_else(|| dimensions_of_space(space))
                .unwrap_or(0),
            ..Space::default()
        };
        for (id, revision) in &rows {
            if let Some(vector) = previous
                .as_ref()
                .and_then(|old| old.vector_for(id, *revision))
            {
                next.push(id, *revision, vector);
                continue;
            }
            let Some((dimensions, bytes)) = store.embedding_blob(id, space)? else {
                continue;
            };
            let vector = decode(&bytes)?;
            if vector.len() != dimensions {
                return Err(HostError::Storage(
                    ("stored embedding dimensions do not match its length").into(),
                ));
            }
            if next.dimensions == 0 {
                next.dimensions = dimensions;
            }
            if dimensions != next.dimensions {
                // A vector of another dimension is not part of this space.
                continue;
            }
            next.push(id, *revision, &vector);
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
        limit: usize,
    ) -> Vec<(String, f32)> {
        let Some(current) = self.spaces.get(&(companion.to_owned(), space.to_owned())) else {
            return Vec::new();
        };
        if query.len() != current.dimensions || limit == 0 {
            return Vec::new();
        }
        (0..current.entries.len())
            .map(|index| {
                let vector = current.vector(index).unwrap_or_default();
                let similarity = cosine_similarity(query, vector);
                let score = if similarity.is_finite() {
                    similarity.clamp(-1.0, 1.0) as f32
                } else {
                    0.0
                };
                (index, score)
            })
            .k_smallest_by(limit, |(left, left_score), (right, right_score)| {
                right_score
                    .total_cmp(left_score)
                    .then_with(|| left.cmp(right))
            })
            .map(|(index, score)| (current.entries[index].0.clone(), score))
            .collect()
    }
}

pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    // Match Rig 0.46's f64 reduction order without widening the stored vectors.
    let pair_sum = |a: &[f32], b: &[f32], term: fn(f64, f64) -> f64| {
        a.chunks(256)
            .zip(b.chunks(256))
            .map(|(a, b)| {
                a.iter()
                    .zip(b)
                    .map(|(&x, &y)| term(f64::from(x), f64::from(y)))
                    .sum::<f64>()
            })
            .sum::<f64>()
    };
    let dot = pair_sum(a, b, |x, y| x * y);
    let magnitude_a = pair_sum(a, a, |x, _| x.powi(2)).sqrt();
    let magnitude_b = pair_sum(b, b, |x, _| x.powi(2)).sqrt();
    dot / (magnitude_a * magnitude_b)
}
