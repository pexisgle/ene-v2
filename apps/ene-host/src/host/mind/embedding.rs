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
    pin::Pin,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

/// The preprocessing version in the composite space id. Bump it whenever the
/// text fed to the model changes, so old vectors cannot be compared.
pub(crate) const PREP_VERSION: &str = "v1";
const MAX_DIMENSIONS: usize = 65_536;
const EMBED_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const SYNC_INTERVAL: Duration = Duration::from_secs(5);

/// A boxed future, so `EmbeddingSource` stays object-safe without adding
/// async-trait.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The composite key that identifies an embedding space: the model, its
/// dimensions and the text preprocessing version.
pub(crate) fn space_id(model: &str, dimensions: usize) -> String {
    format!("{model}:{dimensions}:{PREP_VERSION}")
}

/// Reads the dimensions back from a composite space id.
pub(crate) fn dimensions_of_space(space: &str) -> Option<usize> {
    let (rest, version) = space.rsplit_once(':')?;
    if version != PREP_VERSION {
        return None;
    }
    let (_, dimensions) = rest.rsplit_once(':')?;
    dimensions.parse().ok()
}

/// Everything needed to call one embedding endpoint. Built on the host
/// thread and moved into a background job.
#[derive(Clone, Debug)]
pub(crate) struct EmbeddingTarget {
    #[allow(dead_code)]
    pub(crate) connection_id: String,
    pub(crate) model_id: String,
    pub(crate) dimensions: usize,
    pub(crate) endpoint: String,
    pub(crate) secret: Option<String>,
    pub(crate) space: String,
}

/// A pluggable embedding function: the real implementation speaks HTTP, and
/// tests inject an in-memory one so no test opens a socket.
pub(crate) trait EmbeddingSource: Send + Sync {
    /// Embeds `texts` in the space `target` names.
    fn embed(&self, target: &EmbeddingTarget, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// As `embed`, for a background task that must not block its runtime.
    fn embed_async<'a>(
        &'a self,
        target: &'a EmbeddingTarget,
        texts: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(async move { self.embed(target, texts) })
    }
}

/// The OpenAI-compatible `/embeddings` client. The wire format is the only
/// one this alpha knows; adapters with another shape are not configured
/// instead of being guessed.
pub(crate) struct HttpEmbeddingSource {
    runtime: tokio::runtime::Handle,
}

impl HttpEmbeddingSource {
    pub(crate) fn new(runtime: tokio::runtime::Handle) -> Self {
        Self { runtime }
    }

    async fn request(&self, target: &EmbeddingTarget, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", target.endpoint.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(EMBED_TIMEOUT)
            .build()
            .map_err(|error| HostError::unavailable(format!("embedding client: {error}")))?;
        let body = serde_json::json!({
            "model": target.model_id,
            "input": texts,
            "dimensions": target.dimensions,
        });
        let mut request = client
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&body);
        if let Some(secret) = &target.secret {
            request = request.bearer_auth(secret);
        }
        let response = request
            .send()
            .await
            .map_err(|error| HostError::unavailable(format!("embedding endpoint: {error}")))?;
        if !response.status().is_success() {
            return Err(HostError::unavailable(format!(
                "embedding endpoint answered {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| HostError::unavailable(format!("embedding body: {error}")))?;
        if bytes.len() > MAX_RESPONSE_BYTES {
            return Err(HostError::unavailable(
                "embedding endpoint answered more data than the host reads",
            ));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| HostError::unavailable(format!("embedding body: {error}")))?;
        let data = value
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| HostError::unavailable("embedding body has no data"))?;
        let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (position, item) in data.iter().enumerate() {
            let index = item
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map_or(position, |index| index as usize);
            let vector = item
                .get("embedding")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| HostError::unavailable("embedding item has no vector"))?
                .iter()
                .map(|value| {
                    value
                        .as_f64()
                        .map(|value| value as f32)
                        .ok_or_else(|| HostError::unavailable("embedding item is not numeric"))
                })
                .collect::<Result<Vec<f32>>>()?;
            indexed.push((index, vector));
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
}

impl EmbeddingSource for HttpEmbeddingSource {
    fn embed(&self, target: &EmbeddingTarget, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.runtime.block_on(async {
            match tokio::time::timeout(EMBED_TIMEOUT, self.request(target, texts)).await {
                Ok(result) => result,
                Err(_) => Err(HostError::unavailable("embedding request timed out")),
            }
        })
    }

    fn embed_async<'a>(
        &'a self,
        target: &'a EmbeddingTarget,
        texts: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>>> {
        Box::pin(self.request(target, texts))
    }
}

/// The embedding source and the one background runtime the mind owns. The
/// runtime starts on first use, so a host with no embedding or consolidation
/// connection never pays for it.
#[derive(Resource)]
pub(crate) struct Embeddings {
    source: Option<Arc<dyn EmbeddingSource>>,
    runtime: OnceLock<Arc<tokio::runtime::Runtime>>,
    http: OnceLock<Arc<dyn EmbeddingSource>>,
}

impl Default for Embeddings {
    fn default() -> Self {
        Self {
            source: None,
            runtime: OnceLock::new(),
            http: OnceLock::new(),
        }
    }
}

impl Embeddings {
    /// Replaces the HTTP source, for tests and for a future local runtime.
    #[allow(dead_code)]
    pub(crate) fn with_source(source: Arc<dyn EmbeddingSource>) -> Self {
        Self {
            source: Some(source),
            ..Self::default()
        }
    }

    pub(crate) fn source(&self) -> Option<&dyn EmbeddingSource> {
        if let Some(source) = &self.source {
            return Some(source.as_ref());
        }
        let runtime = self.runtime()?;
        Some(
            self.http
                .get_or_init(|| Arc::new(HttpEmbeddingSource::new(runtime.handle().clone())))
                .as_ref(),
        )
    }

    /// An owned handle for a background task.
    pub(crate) fn source_arc(&self) -> Option<Arc<dyn EmbeddingSource>> {
        if let Some(source) = &self.source {
            return Some(Arc::clone(source));
        }
        let runtime = self.runtime()?;
        Some(Arc::clone(self.http.get_or_init(|| {
            Arc::new(HttpEmbeddingSource::new(runtime.handle().clone()))
        })))
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

    fn runtime(&self) -> Option<&Arc<tokio::runtime::Runtime>> {
        if self.runtime.get().is_none() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .ok()
                .map(Arc::new)?;
            let _ = self.runtime.set(runtime);
        }
        self.runtime.get()
    }
}

/// Resolves the companion's `Embedding` assignment to a callable target.
/// `None` means no usable embedding connection is configured; the host then
/// fabricates no vector and recall stays lexical.
pub(crate) fn embedding_target(
    store: &Store,
    credentials: Option<&Credentials>,
    companion: &Companion,
) -> Result<Option<EmbeddingTarget>> {
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
        assignment.model_id.trim().to_owned()
    } else {
        config.model_id.trim().to_owned()
    };
    let dimensions = config.dimensions as usize;
    // The space id is only stable when the dimensions are known up front.
    if model_id.is_empty() || dimensions == 0 || dimensions > MAX_DIMENSIONS {
        return Ok(None);
    }
    let Some(endpoint) = connection_endpoint(&connection) else {
        return Ok(None);
    };
    let Some(secret) = connection_secret(&connection, credentials) else {
        return Ok(None);
    };
    Ok(Some(EmbeddingTarget {
        connection_id: connection.id,
        model_id: model_id.clone(),
        dimensions,
        endpoint,
        secret: Some(secret),
        space: space_id(&model_id, dimensions),
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

/// A vector row waiting for an embedding.
#[derive(Clone, Debug)]
pub(crate) struct PendingEmbedding {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) content_revision: u64,
}

/// How many memories one embedding call covers.
pub(crate) const MAX_EMBED_BATCH: usize = 16;

/// Embeds the memories whose vector is missing or stale. Called from the host
/// thread by tests and from a background task by the plugin.
#[allow(dead_code)]
pub(crate) fn embed_pending(
    store: &Store,
    source: &dyn EmbeddingSource,
    target: &EmbeddingTarget,
    limit: usize,
) -> Result<usize> {
    let pending = store.memories_missing_embedding(&target.space, limit)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = pending.iter().map(|row| row.content.clone()).collect();
    let vectors = source.embed(target, &texts)?;
    apply_embeddings(store, target, &pending, vectors)
}

/// The asynchronous form used by the background task.
pub(crate) async fn embed_pending_async(
    store: &Store,
    source: &dyn EmbeddingSource,
    target: &EmbeddingTarget,
    limit: usize,
) -> Result<usize> {
    let pending = store.memories_missing_embedding(&target.space, limit)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = pending.iter().map(|row| row.content.clone()).collect();
    let vectors = source.embed_async(target, &texts).await?;
    apply_embeddings(store, target, &pending, vectors)
}

fn apply_embeddings(
    store: &Store,
    target: &EmbeddingTarget,
    pending: &[PendingEmbedding],
    vectors: Vec<Vec<f32>>,
) -> Result<usize> {
    if vectors.len() != pending.len() {
        return Err(HostError::unavailable(
            "embedding endpoint returned the wrong number of vectors",
        ));
    }
    let mut written = 0;
    for (row, vector) in pending.iter().zip(vectors) {
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
            &encode(&vector),
        )? {
            written += 1;
        }
    }
    Ok(written)
}

/// Embeds one query for a hybrid recall.
pub(crate) fn embed_query(
    source: &dyn EmbeddingSource,
    target: &EmbeddingTarget,
    query: &str,
) -> Result<Vec<f32>> {
    let vectors = source.embed(target, &[query.to_owned()])?;
    let vector = vectors
        .into_iter()
        .next()
        .ok_or_else(|| HostError::unavailable("embedding endpoint returned no vector"))?;
    if vector.len() != target.dimensions {
        return Err(HostError::unavailable(
            "embedding dimensions do not match the connection",
        ));
    }
    Ok(vector)
}

/// The f32 LE wire format stored in `memories.embedding`.
pub(crate) fn encode(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<f32>> {
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
pub(crate) fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
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

#[derive(Default)]
struct Space {
    dimensions: usize,
    ids: Vec<String>,
    revisions: Vec<u64>,
    offsets: Vec<usize>,
    vectors: Vec<f32>,
    by_id: HashMap<String, usize>,
}

impl Space {
    fn len(&self) -> usize {
        self.ids.len()
    }

    fn matches(&self, rows: &[(String, u64)]) -> bool {
        self.len() == rows.len()
            && self
                .ids
                .iter()
                .zip(&self.revisions)
                .zip(rows)
                .all(|((id, revision), (row, row_revision))| id == row && revision == row_revision)
    }

    fn push(&mut self, id: &str, revision: u64, vector: &[f32]) -> bool {
        if self.dimensions != 0 && vector.len() != self.dimensions {
            return false;
        }
        if self.dimensions == 0 {
            self.dimensions = vector.len();
        }
        let index = self.ids.len();
        self.offsets.push(self.vectors.len());
        self.ids.push(id.to_owned());
        self.revisions.push(revision);
        self.vectors.extend_from_slice(vector);
        self.by_id.insert(id.to_owned(), index);
        true
    }

    fn vector(&self, index: usize) -> Option<&[f32]> {
        let offset = *self.offsets.get(index)?;
        self.vectors
            .get(offset..offset.saturating_add(self.dimensions))
    }

    fn vector_for(&self, id: &str, revision: u64) -> Option<&[f32]> {
        let index = *self.by_id.get(id)?;
        (self.revisions.get(index) == Some(&revision))
            .then(|| self.vector(index))
            .flatten()
    }

    fn cosine_at(&self, query: &[f32], index: usize) -> Option<f32> {
        if query.len() != self.dimensions {
            return None;
        }
        self.vector(index)
            .map(|vector| cosine_similarity(query, vector))
    }
}

/// The ECS cache of the canonical vectors. Each embedding space keeps one
/// flat `Vec<f32>`; `offsets[i]` is where vector `i` starts, and `ids[i]` is
/// the memory it belongs to.
#[derive(Resource, Default)]
pub(crate) struct CompactVectorIndex {
    spaces: HashMap<String, Space>,
}

impl CompactVectorIndex {
    /// Mirrors one space from SQLite. Returns how many vectors were added,
    /// replaced or removed; `0` means the `(id, memory_revision)` signature
    /// did not change and nothing was read from the BLOB column.
    pub(crate) fn sync(&mut self, store: &Store, space: &str) -> Result<usize> {
        let rows = store.embedding_revisions(space)?;
        if let Some(current) = self.spaces.get(space)
            && current.matches(&rows)
        {
            return Ok(0);
        }
        let previous = self.spaces.get(space);
        let mut next = Space {
            dimensions: previous
                .map(|current| current.dimensions)
                .or_else(|| dimensions_of_space(space))
                .unwrap_or(0),
            ..Space::default()
        };
        let mut seen = HashSet::new();
        let mut changed = 0;
        for (id, revision) in &rows {
            seen.insert(id.as_str());
            if let Some(vector) = previous.and_then(|old| old.vector_for(id, *revision)) {
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
            changed += 1;
        }
        if let Some(previous) = previous {
            changed += previous
                .ids
                .iter()
                .filter(|id| !seen.contains(id.as_str()))
                .count();
        }
        self.spaces.insert(space.to_owned(), next);
        Ok(changed)
    }

    /// Mirrors every space that has stored vectors.
    pub(crate) fn sync_all(&mut self, store: &Store) -> Result<()> {
        let spaces = store.embedding_spaces()?;
        for space in &spaces {
            self.sync(store, space)?;
        }
        let live: HashSet<&str> = spaces.iter().map(String::as_str).collect();
        self.spaces.retain(|space, _| live.contains(space.as_str()));
        Ok(())
    }

    /// The cosine ranking of one space, highest first. The scan allocates no
    /// per-candidate buffer; only the returned id list does.
    pub(crate) fn ranked(&self, space: &str, query: &[f32], limit: usize) -> Vec<(String, f32)> {
        let Some(current) = self.spaces.get(space) else {
            return Vec::new();
        };
        self.ranked_indices(space, query, limit)
            .into_iter()
            .filter_map(|(index, score)| current.ids.get(index).map(|id| (id.clone(), score)))
            .collect()
    }

    /// The ranking by index, for callers that already hold the id list.
    pub(crate) fn ranked_indices(
        &self,
        space: &str,
        query: &[f32],
        limit: usize,
    ) -> Vec<(usize, f32)> {
        let Some(current) = self.spaces.get(space) else {
            return Vec::new();
        };
        let mut scored: Vec<(usize, f32)> = (0..current.len())
            .filter_map(|index| current.cosine_at(query, index).map(|score| (index, score)))
            .collect();
        scored.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        scored.truncate(limit);
        scored
    }

    #[allow(dead_code)]
    pub(crate) fn dimensions(&self, space: &str) -> Option<usize> {
        self.spaces
            .get(space)
            .map(|current| current.dimensions)
            .filter(|dimensions| *dimensions > 0)
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self, space: &str) -> usize {
        self.spaces.get(space).map_or(0, Space::len)
    }

    #[allow(dead_code)]
    pub(crate) fn spaces(&self) -> Vec<String> {
        self.spaces.keys().cloned().collect()
    }
}

/// Keeps the resource in step with the database. It is a safety net; recall
/// may call `CompactVectorIndex::sync` itself before ranking.
pub(crate) fn register(app: &mut App) {
    app.init_resource::<Embeddings>()
        .init_resource::<CompactVectorIndex>()
        .init_resource::<EmbeddingJobs>()
        .add_systems(Update, (sync_index, backfill_embeddings).after(AgentLoop));
}

/// The companion ids with an embedding pass in flight, plus the result
/// channel the background task posts to.
#[derive(Resource)]
pub(crate) struct EmbeddingJobs {
    running: HashSet<String>,
    sender: crossbeam_channel::Sender<EmbeddingDone>,
    receiver: crossbeam_channel::Receiver<EmbeddingDone>,
}

impl Default for EmbeddingJobs {
    fn default() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(16);
        Self {
            running: HashSet::new(),
            sender,
            receiver,
        }
    }
}

pub(crate) enum EmbeddingDone {
    Done {
        companion_id: String,
        written: usize,
    },
    Failed {
        companion_id: String,
        error: String,
    },
}

const BACKFILL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BACKFILL: usize = 256;

/// Embeds what is missing for each companion with a usable connection. The
/// database is written from the task through the shared `Store`, one short
/// lock per row group; the ECS thread only schedules and drains.
fn backfill_embeddings(
    store: Res<Store>,
    embeddings: Res<Embeddings>,
    credentials: Option<Res<Credentials>>,
    mut jobs: ResMut<EmbeddingJobs>,
    mut last: Local<Option<Instant>>,
    mut notifier: Option<ResMut<crate::host::notify::Notifier>>,
) {
    while let Ok(done) = jobs.receiver.try_recv() {
        match done {
            EmbeddingDone::Done {
                companion_id,
                written,
            } => {
                jobs.running.remove(&companion_id);
                if written > 0 {
                    tracing::debug!(companion = %companion_id, written, "embeddings stored");
                }
            }
            EmbeddingDone::Failed {
                companion_id,
                error,
            } => {
                jobs.running.remove(&companion_id);
                if let Some(notifier) = notifier.as_mut() {
                    notifier.notice(
                        Some(&companion_id),
                        "embedding_failed",
                        &crate::host::db::truncate(&error, 300),
                    );
                }
            }
        }
    }
    let now = Instant::now();
    if last.is_some_and(|last| now.duration_since(last) < BACKFILL_INTERVAL) {
        return;
    }
    *last = Some(now);
    let Ok(companions) = store.companions() else {
        return;
    };
    for companion in companions {
        if jobs.running.contains(&companion.id) {
            continue;
        }
        let Ok(Some(target)) = embedding_target(&store, credentials.as_deref(), &companion) else {
            continue;
        };
        match store.memories_missing_embedding(&target.space, 1) {
            Ok(pending) if pending.is_empty() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        let Some(source) = embeddings.source_arc() else {
            continue;
        };
        let companion_id = companion.id.clone();
        let sender = jobs.sender.clone();
        jobs.running.insert(companion_id.clone());
        let store = store.clone();
        let queued = embeddings.spawn(async move {
            let mut written = 0;
            while written < MAX_BACKFILL {
                match embed_pending_async(&store, source.as_ref(), &target, MAX_EMBED_BATCH).await {
                    Ok(0) => break,
                    Ok(count) => written += count,
                    Err(error) => {
                        let _ = sender.send(EmbeddingDone::Failed {
                            companion_id,
                            error: error.to_string(),
                        });
                        return;
                    }
                }
            }
            let _ = sender.send(EmbeddingDone::Done {
                companion_id,
                written,
            });
        });
        if !queued {
            jobs.running.remove(&companion.id);
        }
    }
}

fn sync_index(
    store: Res<Store>,
    mut index: ResMut<CompactVectorIndex>,
    mut last: Local<Option<Instant>>,
) {
    let now = Instant::now();
    if last.is_some_and(|last| now.duration_since(last) < SYNC_INTERVAL) {
        return;
    }
    *last = Some(now);
    let _ = index.sync_all(&store);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::memories::NewMemory;

    struct Fake {
        vectors: std::sync::Mutex<Vec<Vec<f32>>>,
        calls: std::sync::Mutex<usize>,
    }

    impl Fake {
        fn new(vectors: Vec<Vec<f32>>) -> Self {
            Self {
                vectors: std::sync::Mutex::new(vectors),
                calls: std::sync::Mutex::new(0),
            }
        }
    }

    impl EmbeddingSource for Fake {
        fn embed(&self, _target: &EmbeddingTarget, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            *self.calls.lock().unwrap() += 1;
            let vectors = self.vectors.lock().unwrap();
            if vectors.len() < texts.len() {
                return Err(HostError::unavailable("fake has too few vectors"));
            }
            Ok(vectors[..texts.len()].to_vec())
        }
    }

    fn store() -> Store {
        let temp = tempfile::tempdir().unwrap();
        Store::open(&temp.path().join("host.sqlite3")).unwrap()
    }

    fn target(dimensions: usize) -> EmbeddingTarget {
        EmbeddingTarget {
            connection_id: "embed".into(),
            model_id: "fake".into(),
            dimensions,
            endpoint: "http://localhost:1".into(),
            secret: None,
            space: space_id("fake", dimensions),
        }
    }

    fn memory(content: &str) -> NewMemory {
        NewMemory {
            id: uuid::Uuid::new_v4().to_string(),
            owner: "ene".into(),
            audiences: Vec::new(),
            content: content.into(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    #[test]
    fn cosine_is_bounded_and_allocation_free_for_the_scan() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(dimensions_of_space("fake:4:v1"), Some(4));
        assert_eq!(dimensions_of_space("fake:4:v2"), None);
    }

    #[test]
    fn the_index_mirrors_sqlite_and_follows_revision_changes() {
        let store = store();
        let space = space_id("fake", 2);
        let first = store.insert_memory(memory("tea")).unwrap();
        let second = store.insert_memory(memory("coffee")).unwrap();
        store
            .set_embedding(
                &first.id,
                first.content_revision,
                &space,
                2,
                &encode(&[1.0, 0.0]),
            )
            .unwrap();
        store
            .set_embedding(
                &second.id,
                second.content_revision,
                &space,
                2,
                &encode(&[0.0, 1.0]),
            )
            .unwrap();
        let mut index = CompactVectorIndex::default();
        assert_eq!(index.sync(&store, &space).unwrap(), 2);
        assert_eq!(index.sync(&store, &space).unwrap(), 0);
        assert_eq!(index.dimensions(&space), Some(2));
        let query = [1.0f32, 0.1];
        let ranked = index.ranked(&space, &query, 8);
        assert_eq!(ranked[0].0, first.id);
        assert!(ranked[0].1 > ranked[1].1);
        // The index ranking is the ranking of the SQLite BLOBs themselves.
        let mut direct: Vec<(String, f32)> = [&first.id, &second.id]
            .into_iter()
            .map(|id| {
                let (_, stored) = store.embedding_blob(id, &space).unwrap().unwrap();
                (
                    id.clone(),
                    cosine_similarity(&query, &decode(&stored).unwrap()),
                )
            })
            .collect();
        direct.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.cmp(&right.0))
        });
        assert_eq!(
            ranked.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            direct.iter().map(|(id, _)| id).collect::<Vec<_>>()
        );
        for ((_, indexed), (_, direct)) in ranked.iter().zip(&direct) {
            assert!((indexed - direct).abs() < 1e-6);
        }

        // A content change invalidates the embedding; the next sync drops it.
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE memories SET content = 'green tea', content_revision = content_revision + 1, \
                 memory_revision = memory_revision + 1 WHERE id = ?1",
                [&first.id],
            )
            .unwrap();
        assert_eq!(index.sync(&store, &space).unwrap(), 1);
        assert_eq!(index.len(&space), 1);
    }

    #[test]
    fn pending_embeddings_are_written_with_a_content_revision_check() {
        let store = store();
        let target = target(2);
        let memory = store.insert_memory(memory("tea")).unwrap();
        let fake = Fake::new(vec![vec![0.5, 0.5]]);
        assert_eq!(embed_pending(&store, &fake, &target, 8).unwrap(), 1);
        let (dimensions, bytes) = store
            .embedding_blob(&memory.id, &target.space)
            .unwrap()
            .unwrap();
        assert_eq!(dimensions, 2);
        assert_eq!(decode(&bytes).unwrap(), vec![0.5, 0.5]);
        assert_eq!(embed_pending(&store, &fake, &target, 8).unwrap(), 0);

        // A stale content revision is not overwritten.
        store
            .conn()
            .unwrap()
            .execute(
                "UPDATE memories SET content_revision = content_revision + 1 WHERE id = ?1",
                [&memory.id],
            )
            .unwrap();
        assert_eq!(embed_pending(&store, &fake, &target, 8).unwrap(), 1);
        assert_eq!(*fake.calls.lock().unwrap(), 2);
    }

    #[test]
    fn a_wrong_dimension_is_refused_instead_of_stored() {
        let store = store();
        let target = target(3);
        store.insert_memory(memory("tea")).unwrap();
        let fake = Fake::new(vec![vec![1.0, 0.0]]);
        assert!(embed_pending(&store, &fake, &target, 8).is_err());
        assert!(store.embedding_spaces().unwrap().is_empty());
    }

    #[test]
    fn a_configured_embedding_connection_becomes_a_stable_space() {
        let store = store();
        let config = serde_json::json!({
            "config": "embedding",
            "endpoint": {
                "adapter": "openai",
                "endpoint": "http://127.0.0.1:1234/v1",
                "options_schema": "",
                "options": "{}"
            },
            "model_id": "fake-embed",
            "dimensions": 2
        });
        store
            .conn()
            .unwrap()
            .execute(
                "INSERT INTO connections                  (id, name, enabled, kind, config, credential_state, revision, updated_at)                  VALUES ('emb', 'embed', 1, 'embedding', ?1, 'absent', 1, 'now')",
                [config.to_string()],
            )
            .unwrap();
        let mut companion = Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "core".into(),
            profile: Default::default(),
            profile_revision: 1,
            state: Default::default(),
            state_revision: 1,
            body: crate::host::json::JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: crate::host::json::JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Default::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        };
        companion.assignments.embedding = Some(crate::host::mind::Assignment {
            connection_id: "emb".into(),
            model_id: "ignored".into(),
        });
        let target = embedding_target(&store, None, &companion)
            .unwrap()
            .expect("a usable target");
        assert_eq!(target.model_id, "fake-embed");
        assert_eq!(target.dimensions, 2);
        assert_eq!(target.space, "fake-embed:2:v1");
        assert_eq!(target.endpoint, "http://127.0.0.1:1234/v1");
    }

    #[test]
    fn no_embedding_assignment_means_no_vectors() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let companion = Companion {
            id: "ene".into(),
            name: "ene".into(),
            mind: "core".into(),
            profile: Default::default(),
            profile_revision: 1,
            state: Default::default(),
            state_revision: 1,
            body: crate::host::json::JsonDoc::new("ene.body.v1", serde_json::json!({})),
            voice: crate::host::json::JsonDoc::new("ene.voice.v1", serde_json::json!({})),
            assignments: Default::default(),
            persona: None,
            voice_identifier: None,
            revision: 1,
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert!(
            embedding_target(&store, None, &companion)
                .unwrap()
                .is_none()
        );
    }
}
