//! Embeddings: the connection that generates them, the f32 LE BLOBs SQLite
//! keeps as the canonical copy, and the flat in-memory index recall scans.
//!
//! The database stays canonical: every vector is written back through
//! the backfill writer, and the index only mirrors rows whose
//! `embedding_content_revision` still matches their `content_revision`. A
//! memory whose content changed is re-embedded, never mixed with the stale
//! vector (docs/design/mind.md §4.1).
//!
//! # Why the embedding call is raw HTTP
//!
//! rig-core 0.46's provider registry connects completion models only:
//! `Catalog::connect_with` returns `DynModel<Completion>`, and neither
//! `Catalog` nor `ConnectOptions` has an embedding connector. Provider clients
//! do expose `.embedding()` (`providers::openai`, `gemini`, `cohere`, `ollama`
//! and `voyageai`), but each speaks its own wire and is built from that
//! provider's concrete client. Using one would either pin embeddings to one
//! vendor or duplicate the host's provider-neutral "adapter + base URL"
//! connection configuration per provider. The one cross-provider shape this
//! host supports is the OpenAI-compatible `POST {base}/embeddings`, so this
//! module keeps that call generic: one shared client, no per-call client
//! construction, and a hard bound on the response body.
use super::{Companion, Role};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    model::Credentials,
    net,
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// The preprocessing version in the composite space id. Bump it whenever the
/// text fed to the model changes, so old vectors cannot be compared.
const PREP_VERSION: &str = "v1";
const MAX_DIMENSIONS: usize = 65_536;
const EMBED_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const BACKFILL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKFILL: usize = 256;
const MAX_EMBED_BATCH: usize = 16;

/// The composite key that identifies an embedding space: the model, its
/// dimensions and the text preprocessing version.
fn space_id(model: &str, dimensions: usize) -> String {
    format!("{model}:{dimensions}:{PREP_VERSION}")
}

/// Reads the dimensions back from a composite space id. A space of another
/// preprocessing version is not comparable and therefore not this space.
pub(crate) fn dimensions_of_space(space: &str) -> Option<usize> {
    let (rest, version) = space.rsplit_once(':')?;
    if version != PREP_VERSION {
        return None;
    }
    let (_, dimensions) = rest.rsplit_once(':')?;
    dimensions.parse().ok()
}

/// Everything needed to call one embedding endpoint.
struct Target {
    model_id: String,
    dimensions: usize,
    endpoint: String,
    secret: String,
    space: String,
}

/// A vector row waiting for an embedding.
#[derive(Clone, Debug)]
pub(crate) struct PendingEmbedding {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) content_revision: u64,
}

/// POSTs a JSON body to an OpenAI-compatible endpoint and returns its JSON
/// answer. The one wire shape the host reads; another format is not guessed.
pub(crate) async fn post_json(
    url: &str,
    body: &serde_json::Value,
    secret: &str,
    timeout: Duration,
    max_bytes: usize,
    what: &str,
) -> Result<serde_json::Value> {
    net::guard_endpoint(url, what)
        .await
        .map_err(HostError::invalid)?;
    let response = net::client(timeout)
        .map_err(HostError::unavailable)?
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .bearer_auth(secret)
        .json(body)
        .send()
        .await
        .map_err(|error| HostError::unavailable(format!("{what} endpoint: {error}")))?;
    if !response.status().is_success() {
        return Err(HostError::unavailable(format!(
            "{what} endpoint answered {}",
            response.status()
        )));
    }
    let bytes = net::read_body(response, max_bytes, what)
        .await
        .map_err(HostError::unavailable)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| HostError::unavailable(format!("{what} body: {error}")))
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(serde::Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    index: Option<usize>,
}

/// Embeds `texts` in the space `target` names.
async fn request(target: &Target, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let body = serde_json::json!({
        "model": target.model_id,
        "input": texts,
        "dimensions": target.dimensions,
    });
    let value = post_json(
        &format!("{}/embeddings", target.endpoint.trim_end_matches('/')),
        &body,
        &target.secret,
        EMBED_TIMEOUT,
        MAX_RESPONSE_BYTES,
        "embedding",
    )
    .await?;
    let response: EmbeddingResponse = serde_json::from_value(value)
        .map_err(|error| HostError::unavailable(format!("embedding body: {error}")))?;
    let count = response.data.len();
    let mut seen = std::collections::HashSet::new();
    let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(count);
    for (position, item) in response.data.into_iter().enumerate() {
        let index = item.index.unwrap_or(position);
        if index >= count || !seen.insert(index) {
            return Err(HostError::unavailable(
                "embedding endpoint returned an invalid index",
            ));
        }
        indexed.push((index, item.embedding));
    }
    indexed.sort_by_key(|(index, _)| *index);
    let vectors: Vec<Vec<f32>> = indexed.into_iter().map(|(_, vector)| vector).collect();
    if vectors.len() != texts.len() {
        return Err(HostError::unavailable(
            "embedding endpoint returned the wrong number of vectors",
        ));
    }
    for vector in &vectors {
        if vector.len() != target.dimensions {
            return Err(HostError::unavailable(
                "embedding dimensions do not match the connection",
            ));
        }
    }
    Ok(vectors)
}

/// Embeds one query on the host runtime for the recall path. The caller blocks
/// the ECS thread, so it passes the handle it already has instead of an
/// second copy of the handle on a resource of its own.
fn embed(runtime: &tokio::runtime::Handle, target: &Target, text: &str) -> Result<Vec<f32>> {
    let vector = runtime
        .block_on(request(target, &[text.to_owned()]))?
        .pop()
        .ok_or_else(|| HostError::unavailable("embedding endpoint returned no vector"))?;
    if vector.len() != target.dimensions {
        return Err(HostError::unavailable(
            "embedding dimensions do not match the connection",
        ));
    }
    Ok(vector)
}

/// Resolves the companion's `Embedding` assignment to a callable target.
/// `None` means no usable embedding connection is configured; the host then
/// fabricates no vector and recall stays lexical.
fn target(
    store: &Store,
    credentials: Option<&Credentials>,
    companion: &Companion,
) -> Result<Option<Target>> {
    let Some(assignment) = companion.model(Role::Embedding) else {
        return Ok(None);
    };
    let Some(connection) = store.connection(&assignment.connection_id)? else {
        return Ok(None);
    };
    if !connection.enabled || connection.kind != ConnectionKind::Embedding {
        return Ok(None);
    }
    let ConnectionConfig::Embedding(config) = &connection.config else {
        return Ok(None);
    };
    let model_id = if config.model_id.trim().is_empty() {
        assignment.model_id.trim()
    } else {
        config.model_id.trim()
    };
    let dimensions = config.dimensions as usize;
    // The space id is only stable when the dimensions are known up front.
    if model_id.is_empty() || dimensions == 0 || dimensions > MAX_DIMENSIONS {
        return Ok(None);
    }
    let (Some(endpoint), Some(secret)) = (
        crate::host::settings::endpoint_of(&connection),
        crate::host::model::connection_secret(&connection, credentials)
            .map(|secret| secret.value().to_owned()),
    ) else {
        return Ok(None);
    };
    Ok(Some(Target {
        model_id: model_id.to_owned(),
        dimensions,
        endpoint,
        secret,
        space: space_id(model_id, dimensions),
    }))
}

/// The semantic candidates for one query, or none when no embedding space is
/// configured or the caller has no runtime to embed on. A failure degrades to
/// lexical recall; it never fails the turn.
pub(crate) fn semantic_ids(
    store: &Store,
    runtime: Option<&tokio::runtime::Handle>,
    index: &mut CompactVectorIndex,
    credentials: Option<&Credentials>,
    companion: &str,
    query: &str,
) -> Vec<String> {
    let Some(runtime) = runtime else {
        return Vec::new();
    };
    let Ok(Some(record)) = store.companion(companion) else {
        return Vec::new();
    };
    let Ok(Some(target)) = target(store, credentials, &record) else {
        return Vec::new();
    };
    let Ok(vector) = embed(runtime, &target, query) else {
        return Vec::new();
    };
    if index.sync(store, companion, &target.space).is_err() {
        return Vec::new();
    }
    index
        .ranked(companion, &target.space, &vector, 20)
        .into_iter()
        .map(|(id, _)| id)
        .collect()
}

/// The f32 LE wire format stored in `memories.embedding`.
fn encode(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return Err(HostError::storage("stored embedding is corrupt"));
    }
    let (chunks, _) = bytes.as_chunks::<4>();
    Ok(chunks
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect())
}

/// Cosine similarity over two slices, with no allocation. A zero norm, a
/// length mismatch or a non-finite component is `0.0`: the caller treats it
/// as no evidence rather than inventing one.
fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut left_norm = 0.0f32;
    let mut right_norm = 0.0f32;
    for (a, b) in left.iter().zip(right) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    let similarity = dot / (left_norm.sqrt() * right_norm.sqrt());
    if similarity.is_finite() {
        similarity.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

/// One embedding space as one contiguous `Vec<f32>`: `entries[i]` is the
/// memory and revision vector `i` belongs to, stored at `i * dimensions`.
#[derive(Default)]
struct Space {
    dimensions: usize,
    entries: Vec<(String, u64)>,
    vectors: Vec<f32>,
    by_id: HashMap<String, usize>,
}

impl Space {
    fn matches(&self, rows: &[(String, u64)]) -> bool {
        self.entries.as_slice() == rows
    }

    fn push(&mut self, id: &str, revision: u64, vector: &[f32]) -> bool {
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

/// One candidate of a ranking. `Ord` is its rank: a higher score is better,
/// and on a tie the earlier entry is.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    score: f32,
    index: usize,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Candidate {}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.index.cmp(&self.index))
    }
}

/// The ECS cache of the canonical vectors. Each `(owner, space)` keeps one
/// flat `Vec<f32>`, so another companion's vectors can never take the top-k
/// slots of the owner's ranking; `ranked` keeps only the best `limit` without
/// sorting or allocating for the rest.
#[derive(Resource, Default)]
pub(crate) struct CompactVectorIndex {
    spaces: HashMap<(String, String), Space>,
}

impl CompactVectorIndex {
    /// Mirrors one owner's space from SQLite. Vectors whose content revision
    /// still matches are reused, so an unchanged space reads no BLOB column.
    pub(crate) fn sync(&mut self, store: &Store, owner: &str, space: &str) -> Result<()> {
        let rows = store.embedding_revisions(space, owner)?;
        let key = (owner.to_owned(), space.to_owned());
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
                return Err(HostError::storage(
                    "stored embedding dimensions do not match its length",
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

    /// The cosine ranking of one owner's space, highest first. Only the best
    /// `limit` candidates are kept, so the scan neither sorts the whole space
    /// nor allocates an entry per memory.
    pub(crate) fn ranked(
        &self,
        owner: &str,
        space: &str,
        query: &[f32],
        limit: usize,
    ) -> Vec<(String, f32)> {
        let Some(current) = self.spaces.get(&(owner.to_owned(), space.to_owned())) else {
            return Vec::new();
        };
        if query.len() != current.dimensions || limit == 0 {
            return Vec::new();
        }
        let mut best: std::collections::BinaryHeap<std::cmp::Reverse<Candidate>> =
            std::collections::BinaryHeap::with_capacity(limit.min(current.entries.len()));
        for index in 0..current.entries.len() {
            let score = current
                .vector(index)
                .map_or(0.0, |vector| cosine_similarity(query, vector));
            let candidate = Candidate { score, index };
            if best.len() < limit {
                best.push(std::cmp::Reverse(candidate));
            } else if best
                .peek()
                .is_some_and(|std::cmp::Reverse(worst)| candidate > *worst)
            {
                best.pop();
                best.push(std::cmp::Reverse(candidate));
            }
        }
        best.into_sorted_vec()
            .into_iter()
            .filter_map(|std::cmp::Reverse(candidate)| {
                current
                    .entries
                    .get(candidate.index)
                    .map(|(id, _)| (id.clone(), candidate.score))
            })
            .collect()
    }
}

/// The companion ids with a backfill in flight and the failures to report.
#[derive(Resource)]
pub(crate) struct Backfill {
    running: HashSet<String>,
    last: Option<Instant>,
    sender: crossbeam_channel::Sender<(String, Result<usize>)>,
    receiver: crossbeam_channel::Receiver<(String, Result<usize>)>,
}

impl Default for Backfill {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(16);
        Self {
            running: HashSet::new(),
            last: None,
            sender,
            receiver,
        }
    }
}

/// Embeds what is missing for each companion with a usable connection. Each
/// task writes through its own SQLite connection opened from the store's path
/// (docs/design/mind.md §3.2), so the ECS thread's `Mutex<Connection>` is not
/// held while the background writes; the ECS thread only schedules and
/// reports.
fn backfill(
    store: Res<Store>,
    runtime: Option<Res<crate::host::resources::HostRuntime>>,
    credentials: Option<Res<Credentials>>,
    mut state: ResMut<Backfill>,
    mut notifier: Option<ResMut<crate::host::notify::Notifier>>,
) {
    while let Ok((companion, result)) = state.receiver.try_recv() {
        state.running.remove(&companion);
        match result {
            Ok(written) if written > 0 => {
                tracing::debug!(companion = %companion, written, "embeddings stored");
            }
            Ok(_) => {}
            Err(error) => {
                if let Some(notifier) = notifier.as_mut() {
                    notifier.notice(
                        Some(&companion),
                        "embedding_failed",
                        &crate::host::db::truncate(&error.to_string(), 300),
                    );
                }
            }
        }
    }
    let now = Instant::now();
    if state
        .last
        .is_some_and(|last| now.duration_since(last) < BACKFILL_INTERVAL)
    {
        return;
    }
    state.last = Some(now);
    let Ok(companions) = store.companions() else {
        return;
    };
    for companion in companions {
        if state.running.contains(&companion.id) {
            continue;
        }
        let Ok(Some(target)) = target(&store, credentials.as_deref(), &companion) else {
            continue;
        };
        match store.memories_missing_embedding(&target.space, 1) {
            Ok(pending) if pending.is_empty() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let companion_id = companion.id.clone();
        state.running.insert(companion_id.clone());
        let sender = state.sender.clone();
        let store = store.clone();
        // A world without the host runtime has nothing to embed on; the
        // running marker is cleared so the next tick retries.
        let Some(runtime) = runtime.as_deref().map(|runtime| runtime.0.clone()) else {
            state.running.remove(&companion.id);
            continue;
        };
        runtime.spawn(async move {
            // The ECS connection is not moved: the task opens its own
            // connection to the same database, with the same WAL pragmas, so
            // the store's lock is never held while it writes.
            let result = match store.open_secondary() {
                Ok(connection) => fill(connection, &target).await,
                Err(error) => Err(error),
            };
            let _ = sender.send((companion_id, result));
        });
    }
}

/// Embeds missing and stale vectors until the batch budget is spent. All
/// reads and writes go through the task's own connection.
async fn fill(connection: rusqlite::Connection, target: &Target) -> Result<usize> {
    let mut written = 0;
    while written < MAX_BACKFILL {
        let pending = crate::host::db::memories::missing_embeddings(
            &connection,
            &target.space,
            MAX_EMBED_BATCH,
        )?;
        if pending.is_empty() {
            break;
        }
        let texts: Vec<String> = pending.iter().map(|row| row.content.clone()).collect();
        let vectors = request(target, &texts).await?;
        if vectors.len() != pending.len() {
            return Err(HostError::unavailable(
                "embedding endpoint returned the wrong number of vectors",
            ));
        }
        for (row, vector) in pending.iter().zip(&vectors) {
            if vector.len() != target.dimensions {
                return Err(HostError::unavailable(
                    "embedding dimensions do not match the connection",
                ));
            }
            if crate::host::db::memories::write_embedding(
                &connection,
                &row.id,
                row.content_revision,
                &target.space,
                target.dimensions,
                &encode(vector),
            )? {
                written += 1;
            }
        }
    }
    Ok(written)
}

/// Registers the vector cache and its backfill.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<CompactVectorIndex>()
        .init_resource::<Backfill>()
        .add_systems(Update, backfill.after(AgentLoop));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::memories::MemoryRecord;
    use crate::host::mind::memory::Memory;

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn memory(content: &str) -> MemoryRecord {
        MemoryRecord {
            id: uuid::Uuid::new_v4().to_string(),
            owner: "ene".into(),
            audiences: Vec::new(),
            content: content.into(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    fn embed(store: &Store, memory: &Memory, space: &str, dimensions: usize, vector: &[f32]) {
        crate::host::db::memories::write_embedding(
            &store.conn().unwrap(),
            &memory.id,
            memory.content_revision,
            space,
            dimensions,
            &encode(vector),
        )
        .unwrap();
    }

    #[test]
    fn the_index_mirrors_only_the_matching_space_and_revision() {
        let store = store();
        let first = store.insert_memory(memory("tea")).unwrap();
        let second = store.insert_memory(memory("coffee")).unwrap();
        let two = space_id("fake", 2);
        let three = space_id("other", 3);
        embed(&store, &first, &two, 2, &[1.0, 0.0]);
        embed(&store, &second, &three, 3, &[0.0, 0.0, 1.0]);
        let mut index = CompactVectorIndex::default();
        index.sync(&store, "ene", &two).unwrap();
        let ranked = index.ranked("ene", &two, &[1.0, 0.1], 8);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, first.id);
        assert!(
            index.ranked("ene", &two, &[1.0, 0.1, 0.0], 8).is_empty(),
            "a query of another dimension is not this space"
        );
        index.sync(&store, "ene", &three).unwrap();
        assert_eq!(
            index.ranked("ene", &three, &[0.0, 1.0, 0.0], 8)[0].0,
            second.id
        );
        assert!(index.ranked("ene", &three, &[1.0, 0.0], 8).is_empty());

        // A content change invalidates the vector; the next sync drops it.
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE memories SET content = 'green tea', \
                 content_revision = content_revision + 1, \
                 memory_revision = memory_revision + 1 WHERE id = ?1",
                [&first.id],
            )
            .unwrap();
        index.sync(&store, "ene", &two).unwrap();
        assert!(index.ranked("ene", &two, &[1.0, 0.1], 8).is_empty());

        // A vector of another preprocessing version is a different space.
        let legacy = "fake:2:v0";
        embed(&store, &second, legacy, 2, &[1.0, 0.0]);
        index.sync(&store, "ene", legacy).unwrap();
        assert!(index.ranked("ene", legacy, &[1.0, 0.0], 8).is_empty());
    }

    #[test]
    fn another_owners_vectors_cannot_take_the_owners_top_k() {
        let store = store();
        let space = space_id("fake", 2);
        let mine = store.insert_memory(memory("mine")).unwrap();
        embed(&store, &mine, &space, 2, &[1.0, 0.0]);
        // Other companions hold equally close vectors for the same query.
        let mut others = Vec::new();
        for index in 0..5 {
            let mut other = memory(&format!("other {index}"));
            other.owner = format!("other-{index}");
            let other = store.insert_memory(other).unwrap();
            embed(&store, &other, &space, 2, &[1.0, 0.0]);
            others.push(other);
        }
        let mut index = CompactVectorIndex::default();
        index.sync(&store, "ene", &space).unwrap();
        let ranked = index.ranked("ene", &space, &[1.0, 0.0], 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, mine.id, "the owner's top-k is not crowded out");
        // Each owner has its own key space: syncing one does not expose
        // another's memories in its ranking.
        index.sync(&store, "other-0", &space).unwrap();
        let theirs = index.ranked("other-0", &space, &[1.0, 0.0], 10);
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].0, others[0].id);
        assert!(
            theirs.iter().all(|(id, _)| id != &mine.id),
            "another owner's vector is never in this owner's space"
        );
    }

    #[test]
    fn the_backfill_connection_writes_without_the_store_lock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("host.sqlite3");
        let store = Store::open(&path).unwrap();
        let memory = store.insert_memory(memory("tea")).unwrap();
        let space = space_id("fake", 2);
        let connection = store.open_secondary().unwrap();
        // The store's own connection is not borrowed for the write.
        let pending =
            crate::host::db::memories::missing_embeddings(&connection, &space, 8).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            crate::host::db::memories::write_embedding(
                &connection,
                &memory.id,
                memory.content_revision,
                &space,
                2,
                &encode(&[1.0, 0.0]),
            )
            .unwrap()
        );
        let stored = store.embedding_blob(&memory.id, &space).unwrap();
        assert_eq!(stored.unwrap().0, 2);
    }
}
