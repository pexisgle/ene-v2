//! Embeddings: the connection that generates them, the f32 LE BLOBs SQLite
//! keeps as the canonical copy, and the flat in-memory index recall scans.
//!
//! The database stays canonical: every vector is written back through
//! `Store::set_embedding`, and the index only mirrors rows whose
//! `embedding_content_revision` still matches their `content_revision`. A
//! memory whose content changed is re-embedded, never mixed with the stale
//! vector (docs/design/mind.md §4.1).
use super::{Companion, Role};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    model::Credentials,
    settings::{Connection, ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_ecs::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::OnceLock,
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|error| HostError::unavailable(format!("{what} client: {error}")))?;
    let response = client
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
    let bytes = response
        .bytes()
        .await
        .map_err(|error| HostError::unavailable(format!("{what} body: {error}")))?;
    if bytes.len() > max_bytes {
        return Err(HostError::unavailable(format!(
            "{what} endpoint answered more data than the host reads"
        )));
    }
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
    let mut indexed: Vec<(usize, Vec<f32>)> = response
        .data
        .into_iter()
        .enumerate()
        .map(|(position, item)| (item.index.unwrap_or(position), item.embedding))
        .collect();
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

/// The one background runtime. It starts on first use, so a host with no
/// embedding or consolidation connection never pays for it.
#[derive(Resource, Default)]
pub(crate) struct Embedder {
    runtime: OnceLock<tokio::runtime::Runtime>,
}

impl Embedder {
    fn runtime(&self) -> Option<&tokio::runtime::Runtime> {
        if self.runtime.get().is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .ok()?;
            let _ = self.runtime.set(runtime);
        }
        self.runtime.get()
    }

    /// Embeds one query synchronously for the recall path.
    fn embed(&self, target: &Target, text: &str) -> Result<Vec<f32>> {
        let runtime = self
            .runtime()
            .ok_or_else(|| HostError::unavailable("the background runtime is unavailable"))?;
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

    /// Spawns one background task. `false` means this host cannot run it.
    pub(crate) fn spawn<F>(&self, future: F) -> bool
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.runtime()
            .map(|runtime| {
                runtime.spawn(future);
            })
            .is_some()
    }
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
        connection_endpoint(&connection),
        connection_secret(&connection, credentials),
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

/// The endpoint a connection calls, with the built-in provider's default when
/// the config leaves it empty.
pub(crate) fn connection_endpoint(connection: &Connection) -> Option<String> {
    let endpoint = connection.endpoint().endpoint.trim().trim_end_matches('/');
    if !endpoint.is_empty() {
        return Some(endpoint.to_owned());
    }
    crate::host::settings::provider(&connection.endpoint().adapter)
        .map(|provider| provider.endpoint)
        .filter(|endpoint| !endpoint.is_empty())
}

/// Resolves a connection's secret: the host cache first, then the provider's
/// environment variable, then the loopback placeholder.
pub(crate) fn connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
) -> Option<String> {
    if let Some(credentials) = credentials
        && let Some(secret) = credentials.secret(&connection.id)
    {
        return Some(secret);
    }
    if let Some(name) = connection.environment_variable()
        && let Ok(secret) = std::env::var(name)
        && !secret.is_empty()
    {
        return Some(secret);
    }
    loopback_endpoint(&connection.endpoint().endpoint).then(|| "ene-local".to_owned())
}

fn loopback_endpoint(endpoint: &str) -> bool {
    let Ok(url) = url::Url::parse(endpoint) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// The semantic candidates for one query, or none when no embedding space is
/// configured. A failure degrades to lexical recall; it never fails the turn.
pub(crate) fn semantic_ids(
    store: &Store,
    embedder: &Embedder,
    index: &mut CompactVectorIndex,
    credentials: Option<&Credentials>,
    companion: &str,
    query: &str,
) -> Vec<String> {
    let Ok(Some(record)) = store.companion(companion) else {
        return Vec::new();
    };
    let Ok(Some(target)) = target(store, credentials, &record) else {
        return Vec::new();
    };
    let Ok(vector) = embedder.embed(&target, query) else {
        return Vec::new();
    };
    if index.sync(store, &target.space).is_err() {
        return Vec::new();
    }
    index
        .ranked(&target.space, &vector, 20)
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

/// The ECS cache of the canonical vectors. Each embedding space keeps one
/// flat `Vec<f32>`; `ranked` scans it without a per-candidate allocation.
#[derive(Resource, Default)]
pub(crate) struct CompactVectorIndex {
    spaces: HashMap<String, Space>,
}

impl CompactVectorIndex {
    /// Mirrors one space from SQLite. Vectors whose content revision still
    /// matches are reused, so an unchanged space reads no BLOB column.
    pub(crate) fn sync(&mut self, store: &Store, space: &str) -> Result<()> {
        let rows = store.embedding_revisions(space)?;
        if self
            .spaces
            .get(space)
            .is_some_and(|current| current.matches(&rows))
        {
            return Ok(());
        }
        let previous = self.spaces.remove(space);
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
        self.spaces.insert(space.to_owned(), next);
        Ok(())
    }

    /// The cosine ranking of one space, highest first.
    pub(crate) fn ranked(&self, space: &str, query: &[f32], limit: usize) -> Vec<(String, f32)> {
        let Some(current) = self.spaces.get(space) else {
            return Vec::new();
        };
        if query.len() != current.dimensions {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = (0..current.entries.len())
            .map(|index| {
                let score = current
                    .vector(index)
                    .map_or(0.0, |vector| cosine_similarity(query, vector));
                (index, score)
            })
            .collect();
        scored.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        scored.truncate(limit);
        scored
            .into_iter()
            .filter_map(|(index, score)| {
                current
                    .entries
                    .get(index)
                    .map(|(id, _)| (id.clone(), score))
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

/// Embeds what is missing for each companion with a usable connection. The
/// database is written from the task through the shared `Store`; the ECS
/// thread only schedules and reports.
fn backfill(
    store: Res<Store>,
    embedder: Res<Embedder>,
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
        let queued = embedder.spawn(async move {
            let result = fill(&store, &target).await;
            let _ = sender.send((companion_id, result));
        });
        if !queued {
            state.running.remove(&companion.id);
        }
    }
}

/// Embeds missing and stale vectors until the batch budget is spent.
async fn fill(store: &Store, target: &Target) -> Result<usize> {
    let mut written = 0;
    while written < MAX_BACKFILL {
        let pending = store.memories_missing_embedding(&target.space, MAX_EMBED_BATCH)?;
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
            if store.set_embedding(
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

/// Registers the vector cache, the HTTP source and its backfill.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<Embedder>()
        .init_resource::<CompactVectorIndex>()
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
        store
            .set_embedding(
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
        index.sync(&store, &two).unwrap();
        let ranked = index.ranked(&two, &[1.0, 0.1], 8);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, first.id);
        assert!(
            index.ranked(&two, &[1.0, 0.1, 0.0], 8).is_empty(),
            "a query of another dimension is not this space"
        );
        index.sync(&store, &three).unwrap();
        assert_eq!(index.ranked(&three, &[0.0, 1.0, 0.0], 8)[0].0, second.id);
        assert!(index.ranked(&three, &[1.0, 0.0], 8).is_empty());

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
        index.sync(&store, &two).unwrap();
        assert!(index.ranked(&two, &[1.0, 0.1], 8).is_empty());

        // A vector of another preprocessing version is a different space.
        let legacy = "fake:2:v0";
        embed(&store, &second, legacy, 2, &[1.0, 0.0]);
        index.sync(&store, legacy).unwrap();
        assert!(index.ranked(legacy, &[1.0, 0.0], 8).is_empty());
    }
}
