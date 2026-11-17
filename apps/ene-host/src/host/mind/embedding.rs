//! Embeddings: the connection that generates them, the f32 LE BLOBs SQLite
//! keeps as the canonical copy, and the flat in-memory index recall scans.
//!
//! The database stays canonical: every vector is written back through
//! the backfill writer, and the index only mirrors rows whose
//! `embedding_content_revision` still matches their `content_revision`. A
//! memory whose content changed is re-embedded, never mixed with the stale
//! vector (docs/design/mind.md §4.1).
//!
//! The embedding model is the provider client's own (`OpenAI::embedding` or
//! `Gemini::embedding`), so the connection's adapter selects the wire and rig
//! encodes and decodes it. The host still owns what the vectors mean: it
//! validates the count and width against the connection, and derives the
//! space id from the model and dimensions the caller configured.
use super::{Companion, Role};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    model::Credentials,
    net,
    settings::{ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use itertools::Itertools;
use rig_core::{driver::DynModel, operation::Embedding, providers::registry::ProviderConfig};
use rig_ecs::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// The preprocessing version in the composite space id. Bump it whenever the
/// text fed to the model changes, so old vectors cannot be compared.
const PREP_VERSION: &str = "v1";
const MAX_DIMENSIONS: usize = 65_536;
const EMBED_TIMEOUT: Duration = Duration::from_secs(20);
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

#[derive(Clone)]
struct Target {
    model: DynModel<Embedding>,
    dimensions: usize,
    space: String,
}

#[derive(Default)]
struct Models {
    revision: u64,
    credential_generation: Option<u64>,
    targets: HashMap<(String, String), Target>,
}

#[derive(Resource, Clone, Default)]
pub(crate) struct Embeddings {
    models: Arc<Mutex<Models>>,
    index: Arc<Mutex<CompactVectorIndex>>,
    /// The plugin providers this host loaded. Empty on a host with none, so a
    /// catalog-only setup behaves exactly as before.
    providers: crate::host::providers::Providers,
}

impl Embeddings {
    /// The vector cache, with the provider registry its connections resolve
    /// through.
    pub(crate) fn with_providers(providers: crate::host::providers::Providers) -> Self {
        Self {
            providers,
            ..Self::default()
        }
    }
}

/// A vector row waiting for an embedding.
#[derive(Clone, Debug)]
pub(crate) struct PendingEmbedding {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) content_revision: u64,
}

/// Embeds `texts` in the space `target` names.
async fn request(target: &Target, texts: &[String]) -> Result<Vec<Vec<f32>>> {
    let response = tokio::time::timeout(EMBED_TIMEOUT, target.model.call(texts.to_vec()))
        .await
        .map_err(|_| {
            HostError::Unavailable(("the embedding endpoint did not answer in time").into())
        })?
        .map_err(|error| HostError::Unavailable(format!("embedding endpoint: {error}")))?;
    if response.embeddings.len() != texts.len() {
        return Err(HostError::Unavailable(
            ("embedding endpoint returned the wrong number of vectors").into(),
        ));
    }
    let mut vectors = Vec::with_capacity(response.embeddings.len());
    for embedding in response.embeddings {
        if embedding.vec.len() != target.dimensions {
            return Err(HostError::Unavailable(
                ("embedding dimensions do not match the connection").into(),
            ));
        }
        // The wire carries f64; the database keeps f32.
        vectors.push(
            embedding
                .vec
                .into_iter()
                .map(|value| value as f32)
                .collect(),
        );
    }
    Ok(vectors)
}

fn connect(
    adapter: &str,
    endpoint: &str,
    secret: Option<String>,
    model_id: &str,
    dimensions: usize,
    providers: &crate::host::providers::Providers,
) -> Result<DynModel<Embedding>> {
    // A plugin provider answers through the host's broker, not through rig's
    // HTTP client, so it is resolved before the catalog is consulted.
    if let Some(running) = providers.running(adapter, ene_plugin::Extension::Embedding) {
        return Ok(super::plugin::embedding(
            running,
            model_id.to_owned(),
            dimensions,
        ));
    }
    let config = crate::host::model::provider_config(adapter, endpoint, secret, providers);
    let http = rig_reqwest::ReqwestClient::from(net::client().map_err(HostError::Unavailable)?);
    // `Embedding` is the provider-neutral operation both clients erase into, so
    // the call below is the same whichever adapter answered.
    let model: DynModel<Embedding> = match config {
        ProviderConfig::OpenAi(config) => config
            .connect(http)
            .embedding(model_id.to_owned(), Some(dimensions))
            .erase(),
        ProviderConfig::Gemini(config) => config
            .connect(http)
            .embedding(model_id.to_owned(), Some(dimensions))
            .erase(),
        ProviderConfig::Anthropic(_) => {
            return Err(HostError::Unavailable(
                ("the Anthropic adapter serves no embeddings").into(),
            ));
        }
    };
    Ok(model)
}

/// Resolves the companion's `Embedding` assignment to a callable target.
/// `None` means no usable embedding connection is configured; the host then
/// fabricates no vector and recall stays lexical.
fn target(
    store: &Store,
    credentials: Option<&Credentials>,
    companion: &Companion,
    embeddings: &Embeddings,
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
    let Some(endpoint) = crate::host::settings::endpoint_of(&connection, &embeddings.providers)
    else {
        return Ok(None);
    };
    let revision = store.settings_revision()?;
    let key = (connection.id.clone(), model_id.to_owned());
    let mut models = crate::host::lock(&embeddings.models);
    let credential_generation = credentials.map(Credentials::generation);
    if models.revision != revision || models.credential_generation != credential_generation {
        models.targets.clear();
        models.revision = revision;
        models.credential_generation = credential_generation;
    }
    if let Some(target) = models.targets.get(&key) {
        return Ok(Some(target.clone()));
    }
    let secret =
        crate::host::model::connection_secret(&connection, credentials, &embeddings.providers)?;
    let model = connect(
        &connection.endpoint().adapter,
        &endpoint,
        secret,
        model_id,
        dimensions,
        &embeddings.providers,
    )?;
    let target = Target {
        model,
        dimensions,
        space: space_id(model_id, dimensions),
    };
    models.targets.insert(key, target.clone());
    Ok(Some(target))
}

pub(crate) async fn semantic_ids(
    store: &Store,
    embeddings: &Embeddings,
    credentials: Option<&Credentials>,
    companion: &str,
    query: &str,
) -> Vec<String> {
    let Ok(Some(record)) = store.companion(companion) else {
        return Vec::new();
    };
    let Ok(Some(target)) = target(store, credentials, &record, embeddings) else {
        return Vec::new();
    };
    let Ok(mut vectors) = request(&target, &[query.to_owned()]).await else {
        return Vec::new();
    };
    let Some(vector) = vectors.pop() else {
        return Vec::new();
    };
    let mut index = crate::host::lock(&embeddings.index);
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

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
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
    embeddings: Res<Embeddings>,
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
        let Ok(Some(target)) = target(&store, credentials.as_deref(), &companion, &embeddings)
        else {
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
        let credentials = credentials.as_deref().cloned();
        let embeddings = embeddings.clone();
        runtime.spawn(async move {
            // The ECS connection is not moved: the task opens its own
            // connection to the same database, with the same WAL pragmas, so
            // the store's lock is never held while it writes.
            let result = match store.open_secondary() {
                Ok(connection) => {
                    fill(
                        connection,
                        &store,
                        &embeddings,
                        credentials.as_ref(),
                        &companion,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let _ = sender.send((companion_id, result));
        });
    }
}

async fn fill(
    connection: rusqlite::Connection,
    store: &Store,
    embeddings: &Embeddings,
    credentials: Option<&Credentials>,
    companion: &Companion,
) -> Result<usize> {
    let mut written = 0;
    while written < MAX_BACKFILL {
        let Some(target) = target(store, credentials, companion, embeddings)? else {
            break;
        };
        let pending = crate::host::db::memories::missing_embeddings(
            &connection,
            &target.space,
            MAX_EMBED_BATCH,
        )?;
        if pending.is_empty() {
            break;
        }
        let texts: Vec<String> = pending.iter().map(|row| row.content.clone()).collect();
        let vectors = request(&target, &texts).await?;
        if vectors.len() != pending.len() {
            return Err(HostError::Unavailable(
                ("embedding endpoint returned the wrong number of vectors").into(),
            ));
        }
        for (row, vector) in pending.iter().zip(&vectors) {
            if vector.len() != target.dimensions {
                return Err(HostError::Unavailable(
                    ("embedding dimensions do not match the connection").into(),
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
    // The registry is already a resource by the time plugins register, so the
    // cache takes the same handle rather than a second copy.
    let providers = app
        .world()
        .get_resource::<crate::host::providers::Providers>()
        .cloned()
        .unwrap_or_default();
    app.insert_resource(Embeddings::with_providers(providers))
        .init_resource::<Backfill>()
        .add_systems(Update, backfill.after(AgentLoop));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::memories::MemoryRecord;
    use crate::host::mind::memory::Memory;

    #[test]
    fn borrowed_cosine_matches_rig_scores_and_ranks() {
        use rig_core::embeddings::{Embedding, distance::VectorDistance};

        let score = |similarity: f64| {
            if similarity.is_finite() {
                similarity.clamp(-1.0, 1.0) as f32
            } else {
                0.0
            }
        };
        for dimensions in [0, 1, 2, 255, 256, 257, 511, 512, 513, 1536] {
            let query: Vec<f32> = (0..dimensions)
                .map(|i| ((i * 17 % 101) as f32 - 50.0) / 13.0)
                .collect();
            let mut space = Space::default();
            let mut expected = Vec::new();
            let embedding = |values: &[f32]| Embedding {
                document: String::new(),
                vec: values.iter().copied().map(f64::from).collect(),
            };
            for candidate in 0..24 {
                let vector: Vec<f32> = (0..dimensions)
                    .map(|i| match candidate {
                        0 => 0.0,
                        1 => f32::NAN,
                        2 => f32::INFINITY,
                        3 => f32::MAX,
                        4 => f32::MIN_POSITIVE,
                        _ => ((i * (candidate + 1) % 97) as f32 - 48.0) / 11.0,
                    })
                    .collect();
                let old = embedding(&query).cosine_similarity(&embedding(&vector), false);
                let new = cosine_similarity(&query, &vector);
                assert!(old.to_bits() == new.to_bits() || (old.is_nan() && new.is_nan()));
                space.push(&candidate.to_string(), 1, &vector);
                expected.push((candidate.to_string(), score(old)));
            }
            expected.sort_by(|(_, left), (_, right)| right.total_cmp(left));
            expected.truncate(9);
            let mut index = CompactVectorIndex::default();
            index.spaces.insert(("owner".into(), "space".into()), space);
            assert_eq!(index.ranked("owner", "space", &query, 9), expected);
            assert!(
                index
                    .ranked("owner", "space", &vec![0.0; dimensions + 1], 9)
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn embedding_credentials_rotate_and_revoke_across_clones() {
        use crate::host::{
            model,
            settings::{Connection, CredentialState, EmbeddingConnection, Endpoint},
            tests::Harness,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let credentials = Credentials::new(true);
        let endpoint_credentials = credentials.clone();
        let endpoint = tokio::spawn(async move {
            let mut headers = Vec::new();
            for count in [1, 1, 1, 1, 1, MAX_EMBED_BATCH, MAX_EMBED_BATCH, 1] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let mut part = [0; 4096];
                    let read = socket.read(&mut part).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&part[..read]);
                }
                let header_end = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let content_length: usize = std::str::from_utf8(&request[..header_end])
                    .unwrap()
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                while request.len() < header_end + content_length {
                    let mut part = [0; 4096];
                    let read = socket.read(&mut part).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&part[..read]);
                }
                let request = String::from_utf8(request).unwrap();
                headers.push(
                    request
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_owned())
                        })
                        .unwrap_or_default(),
                );
                match headers.len() {
                    6 => model::save_secret(
                        &endpoint_credentials,
                        "credential-embedding",
                        "backfill-new-key",
                    )
                    .unwrap(),
                    7 => {
                        model::delete_secret(&endpoint_credentials, "credential-embedding").unwrap()
                    }
                    _ => {}
                }
                let body = serde_json::json!({
                    "data": (0..count).map(|index| serde_json::json!({
                        "embedding": [1.0, 0.0], "index": index
                    })).collect::<Vec<_>>(),
                    "usage": {"prompt_tokens": 1, "total_tokens": 1}
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            headers
        });
        let harness = Harness::new([]);
        let store = harness.store();
        let writer = credentials.clone();
        let embeddings = Embeddings::default();
        let connection = Connection {
            id: "credential-embedding".into(),
            name: "embedding".into(),
            enabled: true,
            kind: ConnectionKind::Embedding,
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: Endpoint {
                    adapter: "custom".into(),
                    endpoint: format!("http://{address}"),
                    options_schema: String::new(),
                    options: String::new(),
                },
                model_id: "credential-model".into(),
                dimensions: 2,
            }),
        };
        store
            .update_settings(store.settings_revision().unwrap(), &[(connection, 0)])
            .unwrap();
        let mut companion = store.companion(&harness.companion).unwrap().unwrap();
        companion.assignments.embedding = Some(super::super::Assignment {
            connection_id: "credential-embedding".into(),
            model_id: "credential-model".into(),
        });
        store
            .update_companion(&companion, companion.revision)
            .unwrap();
        let revision = store.settings_revision().unwrap();
        for secret in [Some("old-key"), Some("new-key"), None] {
            match secret {
                Some(secret) => {
                    model::save_secret(&writer, "credential-embedding", secret).unwrap()
                }
                None => model::delete_secret(&writer, "credential-embedding").unwrap(),
            }
            store
                .set_credential_state(
                    "credential-embedding",
                    if secret.is_some() {
                        CredentialState::Registered
                    } else {
                        CredentialState::Absent
                    },
                )
                .unwrap();
            let target = target(&store, Some(&credentials), &companion, &embeddings)
                .unwrap()
                .unwrap();
            assert_eq!(
                request(&target, &["query".into()]).await.unwrap(),
                [vec![1.0, 0.0]]
            );
        }
        writer.remember("credential-embedding", "direct-key".into());
        let target = target(&store, Some(&credentials), &companion, &embeddings)
            .unwrap()
            .unwrap();
        request(&target, &["query".into()]).await.unwrap();
        writer.forget("credential-embedding");
        let target = self::target(&store, Some(&credentials), &companion, &embeddings)
            .unwrap()
            .unwrap();
        request(&target, &["query".into()]).await.unwrap();
        writer.remember("credential-embedding", "backfill-old-key".into());
        let pending = MAX_EMBED_BATCH * 2 + 1;
        for index in 0..pending {
            let mut record = memory(&format!("memory {index}"));
            record.owner = companion.id.clone();
            store.insert_memory(record).unwrap();
        }
        assert_eq!(
            fill(
                store.open_secondary().unwrap(),
                &store,
                &embeddings,
                Some(&credentials),
                &companion,
            )
            .await
            .unwrap(),
            pending
        );
        assert_eq!(store.settings_revision().unwrap(), revision);
        let headers = tokio::time::timeout(Duration::from_secs(3), endpoint)
            .await
            .unwrap()
            .unwrap();
        println!("embedding credential lifecycle {headers:?}");
        assert_eq!(
            headers,
            [
                "Bearer old-key",
                "Bearer new-key",
                "Bearer",
                "Bearer direct-key",
                "Bearer",
                "Bearer backfill-old-key",
                "Bearer backfill-new-key",
                "Bearer"
            ]
        );
    }

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
    fn ranking_keeps_database_order_for_ties_and_bounds_top_k() {
        let store = store();
        let space = space_id("fake", 2);
        let mut ids = Vec::new();
        for (id, vector) in [
            ("z-first", [2.0, 0.0]),
            ("negative", [-3.0, 0.0]),
            ("a-second", [1.0, 0.0]),
            ("orthogonal", [0.0, 8.0]),
            ("m-third", [20.0, 0.0]),
            ("angled", [3.0, 4.0]),
        ] {
            let mut record = memory(id);
            record.id = id.into();
            let memory = store.insert_memory(record).unwrap();
            embed(&store, &memory, &space, 2, &vector);
            ids.push(memory.id);
        }
        let mut index = CompactVectorIndex::default();
        index.sync(&store, "ene", &space).unwrap();
        let rank = |limit| index.ranked("ene", &space, &[1.0, 0.0], limit);
        assert!(rank(0).is_empty());
        assert_eq!(rank(1), [(ids[0].clone(), 1.0)]);
        assert_eq!(rank(2), [(ids[0].clone(), 1.0), (ids[2].clone(), 1.0)]);
        assert_eq!(
            rank(20),
            [
                (ids[0].clone(), 1.0),
                (ids[2].clone(), 1.0),
                (ids[4].clone(), 1.0),
                (ids[5].clone(), 0.6),
                (ids[3].clone(), 0.0),
                (ids[1].clone(), -1.0),
            ]
        );
        index.sync(&store, "ene", &space).unwrap();
        assert_eq!(index.ranked("ene", &space, &[1.0, 0.0], 2)[1].0, ids[2]);
    }

    #[test]
    fn invalid_comparisons_stay_finite_zero_evidence() {
        let store = store();
        let space = space_id("fake", 2);
        let mut ids = Vec::new();
        for vector in [
            [1.0, 0.0],
            [0.0, 0.0],
            [f32::NAN, 0.0],
            [f32::INFINITY, 0.0],
            [-1.0, 0.0],
        ] {
            let memory = store.insert_memory(memory("vector")).unwrap();
            embed(&store, &memory, &space, 2, &vector);
            ids.push(memory.id);
        }
        let mut index = CompactVectorIndex::default();
        index.sync(&store, "ene", &space).unwrap();
        assert_eq!(
            index.ranked("ene", &space, &[1.0, 0.0], 20),
            [
                (ids[0].clone(), 1.0),
                (ids[1].clone(), 0.0),
                (ids[2].clone(), 0.0),
                (ids[3].clone(), 0.0),
                (ids[4].clone(), -1.0),
            ]
        );
        for query in [[0.0, 0.0], [f32::NAN, 0.0], [f32::INFINITY, 0.0]] {
            assert_eq!(
                index.ranked("ene", &space, &query, 2),
                [(ids[0].clone(), 0.0), (ids[1].clone(), 0.0)]
            );
        }
    }
}
