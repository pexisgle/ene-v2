//! Embeddings: the connection that generates them, the f32 LE BLOBs SQLite
//! keeps as the canonical copy, and the flat in-memory index recall scans.
//!
//! The database stays canonical: every vector is written back through
//! the backfill writer, and the index only mirrors rows whose
//! `embedding_content_revision` still matches their `content_revision`. A
//! memory whose content changed is re-embedded, never mixed with the stale
//! vector (docs/design/mind.md §4.1).
//!
//! Each embedding space hashes non-secret canonical provider parameters
//! (`adapter`, `endpoint`, `options`, `model`) alongside `dimensions` and the
//! preprocessing version, so changing providers, endpoints, or model options
//! never mixes incompatible vector spaces while rotating API keys preserves
//! existing vectors.
mod backfill;
mod index;

pub(crate) use backfill::Backfill;
pub(crate) use index::CompactVectorIndex;

use super::{Companion, Role};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    model::Credentials,
    net,
    settings::{Connection, ConnectionConfig, ConnectionKind},
};
use bevy_ecs::prelude::*;
use rig_core::{driver::DynModel, operation::Embedding, providers::registry::ProviderConfig};
use rig_ecs::prelude::*;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) use ene_mind::{dimensions_of_space, space_id};

const MAX_DIMENSIONS: usize = 65_536;
const EMBED_TIMEOUT: Duration = Duration::from_secs(20);

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

    pub(crate) fn providers(&self) -> crate::host::providers::Providers {
        self.providers.clone()
    }
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

pub(crate) fn default_dimensions(model_id: &str) -> Option<u32> {
    match model_id.trim() {
        "multilingual-e5-large" => Some(1024),
        "preview/Qwen3-Embedding-4B-FP16" | "Qwen3-Embedding-4B" | "Qwen3-Embedding-4B-FP16" => {
            Some(2560)
        }
        "text-embedding-3-small" | "text-embedding-ada-002" => Some(1536),
        "text-embedding-3-large" => Some(3072),
        _ => None,
    }
}

// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_arguments)]
fn connect_with_ndims(
    connection: &Connection,
    credentials: Option<&Credentials>,
    endpoint: &str,
    model_id: &str,
    ndims: Option<usize>,
    plugin_ndims: usize,
    providers: &crate::host::providers::Providers,
) -> Result<DynModel<Embedding>> {
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin provider answers through the host's broker, not through rig's
    // HTTP client, so it is resolved before the catalog is consulted.
    if providers.get(adapter).is_some() {
        let running = providers
            .running(adapter, ene_plugin::Extension::Embedding)
            .ok_or_else(|| {
                HostError::Unavailable(format!("embedding plugin `{adapter}` is not running"))
            })?;
        let context = crate::host::providers::ProviderInvocationContext::from_connection(
            &running,
            connection,
            credentials,
            providers,
            Some(EMBED_TIMEOUT),
        )?;
        return Ok(super::plugin::embedding(
            running,
            context,
            model_id.to_owned(),
            plugin_ndims,
        ));
    }
    let secret = crate::host::model::connection_secret(connection, credentials, providers)?;
    let config = crate::host::model::provider_config(adapter, endpoint, secret, providers);
    let http = rig_reqwest::ReqwestClient::from(net::client().map_err(HostError::Unavailable)?);
    // `Embedding` is the provider-neutral operation both clients erase into, so
    // the call below is the same whichever adapter answered.
    let model: DynModel<Embedding> = match config {
        ProviderConfig::OpenAi(config) => config
            .connect(http)
            .embedding(model_id.to_owned(), ndims)
            .erase(),
        ProviderConfig::Gemini(config) => config
            .connect(http)
            .embedding(model_id.to_owned(), ndims)
            .erase(),
        ProviderConfig::Anthropic(_) => {
            return Err(HostError::Unavailable(
                ("the Anthropic adapter serves no embeddings").into(),
            ));
        }
    };
    Ok(model)
}

fn connect(
    connection: &Connection,
    credentials: Option<&Credentials>,
    endpoint: &str,
    model_id: &str,
    dimensions: usize,
    providers: &crate::host::providers::Providers,
) -> Result<DynModel<Embedding>> {
    connect_with_ndims(
        connection,
        credentials,
        endpoint,
        model_id,
        Some(dimensions),
        dimensions,
        providers,
    )
}

/// Probes an embedding connection by generating one sample vector without forcing
/// `dimensions` on the wire so the host observes the server's actual output width.
pub(crate) async fn probe_dimensions(
    connection: &Connection,
    credentials: &Credentials,
    providers: &crate::host::providers::Providers,
) -> std::result::Result<usize, String> {
    let ConnectionConfig::Embedding(config) = &connection.config else {
        return Err("埋め込み接続ではありません".into());
    };
    let base = crate::host::settings::endpoint_of(connection, providers)
        .ok_or_else(|| "接続先URLが設定されていません".to_owned())?;
    let adapter = connection.endpoint().adapter.as_str();
    if providers.get(adapter).is_some() {
        providers
            .start(adapter, ene_plugin::Extension::Embedding)
            .await
            .map_err(|error| error.to_string())?;
    }
    let sakura = crate::host::model::is_sakura_connection(adapter, &base);
    let model_id = if config.model_id.trim().is_empty() {
        if sakura {
            "multilingual-e5-large"
        } else {
            "text-embedding-3-small"
        }
    } else {
        config.model_id.trim()
    };
    let model = connect_with_ndims(
        connection,
        Some(credentials),
        &base,
        model_id,
        None,
        config.dimensions as usize,
        providers,
    )
    .map_err(|error| error.to_string())?;
    let response = tokio::time::timeout(EMBED_TIMEOUT, model.call(vec!["ping".to_owned()]))
        .await
        .map_err(|_| "埋め込みエンドポイントがタイムアウトしました".to_owned())?
        .map_err(|error| error.to_string())?;
    let first = response
        .embeddings
        .into_iter()
        .next()
        .ok_or_else(|| "埋め込みエンドポイントがベクトルを返しませんでした".to_owned())?;
    Ok(first.vec.len())
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
    let dimensions = if config.dimensions > 0 {
        config.dimensions as usize
    } else {
        default_dimensions(model_id).unwrap_or(0) as usize
    };
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
    let model = connect(
        &connection,
        credentials,
        &endpoint,
        model_id,
        dimensions,
        &embeddings.providers,
    )?;
    let space = space_id(
        &connection.endpoint().adapter,
        &endpoint,
        &connection.endpoint().options,
        model_id,
        dimensions,
    );
    let target = Target {
        model,
        dimensions,
        space,
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
        .add_systems(Update, backfill::backfill.after(AgentLoop));
}

#[cfg(test)]
mod tests {
    use super::backfill::{MAX_EMBED_BATCH, fill};
    use super::index::{Space, cosine_similarity, encode};
    use super::*;
    use crate::host::db::memories::MemoryRecord;
    use crate::host::mind::memory::Memory;

    fn test_space(model: &str, dimensions: usize) -> String {
        space_id(
            "custom",
            "https://example.invalid/v1",
            "{}",
            model,
            dimensions,
        )
    }

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
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
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
            record.scope = companion.id.clone();
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
            scope: "ene".into(),
            content: content.into(),
            importance: 5,
            occurred_at: None,
            source_context: serde_json::json!({}),
        }
    }

    fn embed(store: &Store, memory: &Memory, space: &str, dimensions: usize, vector: &[f32]) {
        Store::write_embedding_on(
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
        let two = test_space("fake", 2);
        let three = test_space("other", 3);
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
    fn another_companions_vectors_cannot_take_the_companions_top_k() {
        let store = store();
        let space = test_space("fake", 2);
        let mine = store.insert_memory(memory("mine")).unwrap();
        embed(&store, &mine, &space, 2, &[1.0, 0.0]);
        // Other companions hold equally close vectors for the same query.
        let mut others = Vec::new();
        for index in 0..5 {
            let mut other = memory(&format!("other {index}"));
            other.scope = format!("other-{index}");
            let other = store.insert_memory(other).unwrap();
            embed(&store, &other, &space, 2, &[1.0, 0.0]);
            others.push(other);
        }
        let mut index = CompactVectorIndex::default();
        index.sync(&store, "ene", &space).unwrap();
        let ranked = index.ranked("ene", &space, &[1.0, 0.0], 1);
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            ranked[0].0, mine.id,
            "the companion's top-k is not crowded out"
        );
        // Each companion has its own key space: syncing one does not expose
        // another's memories in its ranking.
        index.sync(&store, "other-0", &space).unwrap();
        let theirs = index.ranked("other-0", &space, &[1.0, 0.0], 10);
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].0, others[0].id);
        assert!(
            theirs.iter().all(|(id, _)| id != &mine.id),
            "another companion's vector is never in this companion's space"
        );
    }

    #[test]
    fn ranking_keeps_database_order_for_ties_and_bounds_top_k() {
        let store = store();
        let space = test_space("fake", 2);
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
        let space = test_space("fake", 2);
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

    #[test]
    fn q03_space_id_distinguishes_endpoints_adapters_and_options_without_secrets() {
        let base = space_id(
            "openai",
            "https://api.example/v1/",
            r#"{"b":2,"a":1}"#,
            "nomic-embed:latest",
            768,
        );
        let reordered = space_id(
            "openai",
            "https://api.example/v1",
            r#"{ "a": 1, "b": 2 }"#,
            "nomic-embed:latest",
            768,
        );
        assert_eq!(base, reordered);
        assert_eq!(dimensions_of_space(&base), Some(768));

        let diff_endpoint = space_id(
            "openai",
            "https://other.example/v1",
            r#"{"a":1,"b":2}"#,
            "nomic-embed:latest",
            768,
        );
        let diff_adapter = space_id(
            "gemini",
            "https://api.example/v1",
            r#"{"a":1,"b":2}"#,
            "nomic-embed:latest",
            768,
        );
        let diff_options = space_id(
            "openai",
            "https://api.example/v1",
            r#"{"a":1,"b":3}"#,
            "nomic-embed:latest",
            768,
        );
        assert_ne!(base, diff_endpoint);
        assert_ne!(base, diff_adapter);
        assert_ne!(base, diff_options);
    }

    #[tokio::test]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    async fn q03_backfill_scopes_by_companion_and_prevents_vector_overwrites() {
        use crate::host::{
            settings::{Connection, CredentialState, EmbeddingConnection, Endpoint},
            tests::Harness,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn start_fake_embed_server(val: f32) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = socket.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let body = serde_json::json!({
                        "data": [{"embedding": [val, 0.0], "index": 0}],
                        "usage": {"prompt_tokens": 1, "total_tokens": 1}
                    })
                    .to_string();
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(reply.as_bytes()).await;
                }
            });
            format!("http://{addr}")
        }

        let endpoint_a = start_fake_embed_server(1.0).await;
        let endpoint_b = start_fake_embed_server(0.0).await;
        let harness = Harness::new([]);
        let store = harness.store();
        let conn_a = Connection {
            id: "emb-alpha".into(),
            name: "Alpha".into(),
            enabled: true,
            kind: ConnectionKind::Embedding,
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: Endpoint {
                    adapter: "custom".into(),
                    endpoint: endpoint_a.clone(),
                    options_schema: String::new(),
                    options: r#"{"profile":"alpha"}"#.into(),
                },
                model_id: "shared-name".into(),
                dimensions: 2,
            }),
        };
        let conn_b = Connection {
            id: "emb-beta".into(),
            name: "Beta".into(),
            enabled: true,
            kind: ConnectionKind::Embedding,
            credential_state: CredentialState::Absent,
            revision: 0,
            last_check: None,
            config: ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: Endpoint {
                    adapter: "custom".into(),
                    endpoint: endpoint_b.clone(),
                    options_schema: String::new(),
                    options: r#"{"profile":"beta"}"#.into(),
                },
                model_id: "shared-name".into(),
                dimensions: 2,
            }),
        };
        store
            .update_settings(
                store.settings_revision().unwrap(),
                &[(conn_a, 0), (conn_b, 0)],
            )
            .unwrap();

        let mut comp_a = store.companion(&harness.companion).unwrap().unwrap();
        comp_a.assignments.embedding = Some(super::super::Assignment {
            connection_id: "emb-alpha".into(),
            model_id: "shared-name".into(),
        });
        store.update_companion(&comp_a, comp_a.revision).unwrap();

        let mut comp_b = comp_a.clone();
        comp_b.id = "beta".into();
        comp_b.name = "Beta".into();
        comp_b.assignments.embedding = Some(super::super::Assignment {
            connection_id: "emb-beta".into(),
            model_id: "shared-name".into(),
        });
        store.insert_companion(&comp_b).unwrap();

        let mut mem_a = memory("alpha private memory");
        mem_a.scope = comp_a.id.clone();
        let mem_a = store.insert_memory(mem_a).unwrap();

        let mut mem_b = memory("beta private memory");
        mem_b.scope = comp_b.id.clone();
        let mem_b = store.insert_memory(mem_b).unwrap();

        let embeddings = Embeddings::default();
        let target_a = target(&store, None, &comp_a, &embeddings).unwrap().unwrap();
        let target_b = target(&store, None, &comp_b, &embeddings).unwrap().unwrap();
        assert_ne!(target_a.space, target_b.space);

        // Missing query scoped to alpha excludes beta's private memory.
        let missing_a = store
            .memories_missing_embedding(&comp_a.id, &target_a.space, 10)
            .unwrap();
        assert_eq!(
            missing_a.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [mem_a.id.as_str()]
        );

        fill(
            store.open_secondary().unwrap(),
            &store,
            &embeddings,
            None,
            &comp_a,
        )
        .await
        .unwrap();
        assert!(
            store
                .embedding_blob(&mem_a.id, &target_a.space)
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .embedding_blob(&mem_b.id, &target_a.space)
                .unwrap()
                .is_none()
        );

        fill(
            store.open_secondary().unwrap(),
            &store,
            &embeddings,
            None,
            &comp_b,
        )
        .await
        .unwrap();
        assert!(
            store
                .embedding_blob(&mem_b.id, &target_b.space)
                .unwrap()
                .is_some()
        );
        // Alpha's space was not polluted by Beta's backfill.
        let mut index = CompactVectorIndex::default();
        index.sync(&store, &comp_a.id, &target_a.space).unwrap();
        let a_ranked = index.ranked(&comp_a.id, &target_a.space, &[1.0, 0.0], 10);
        assert_eq!(a_ranked.len(), 1);
        assert_eq!(a_ranked[0].0, mem_a.id);
    }

    #[tokio::test]
    async fn sakura_embedding_omits_dimensions_and_decodes_without_usage() {
        use crate::host::settings::{
            Connection, ConnectionConfig, ConnectionKind, CredentialState, EmbeddingConnection,
            Endpoint,
        };
        use crate::host::tests::Harness;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = socket.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&buf[..pos]);
                        let cl = header
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if buf.len() >= pos + 4 + cl {
                            let body_start = pos + 4;
                            let _ = tx.send(
                                String::from_utf8_lossy(&buf[body_start..body_start + cl])
                                    .into_owned(),
                            );
                            break;
                        }
                    }
                }
                let vec1024 = vec![0.25f64; 1024];
                let body = serde_json::json!({
                    "model": "multilingual-e5-large",
                    "data": [{
                        "index": 0,
                        "object": "embedding",
                        "embedding": vec1024,
                    }]
                })
                .to_string();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });

        let credentials = Credentials::new(true);
        credentials.remember("sakura-emb", "uuid:secret".into());
        let connection = Connection {
            id: "sakura-emb".into(),
            name: "Sakura Embedding".into(),
            enabled: true,
            kind: ConnectionKind::Embedding,
            credential_state: CredentialState::Registered,
            revision: 1,
            last_check: None,
            config: ConnectionConfig::Embedding(EmbeddingConnection {
                endpoint: Endpoint {
                    adapter: "sakura".into(),
                    endpoint: format!("http://{addr}/v1"),
                    options_schema: String::new(),
                    options: "{}".into(),
                },
                model_id: "multilingual-e5-large".into(),
                dimensions: 0,
            }),
        };
        let providers = crate::host::providers::Providers::none();
        let actual_dim = probe_dimensions(&connection, &credentials, &providers)
            .await
            .unwrap();
        assert_eq!(actual_dim, 1024);

        let harness = Harness::new([]);
        let store = harness.store();
        let rev = store.settings_revision().unwrap();
        store.update_settings(rev, &[(connection, 0)]).unwrap();
        let mut comp = store.companion(&harness.companion).unwrap().unwrap();
        comp.assignments.embedding = Some(super::super::Assignment {
            connection_id: "sakura-emb".into(),
            model_id: "multilingual-e5-large".into(),
        });
        let embeddings = Embeddings::default();
        let resolved = target(&store, Some(&credentials), &comp, &embeddings)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.dimensions, 1024);
        let vectors = request(&resolved, &["こんにちは".into()]).await.unwrap();
        assert_eq!(vectors.len(), 1);
        assert_eq!(vectors[0].len(), 1024);

        for _ in 0..2 {
            let body_str = rx.recv().unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
            assert_eq!(parsed["model"], "multilingual-e5-large");
            assert!(
                parsed.get("dimensions").is_none(),
                "sakura dialect must omit OpenAI dimensions field: {parsed}"
            );
        }
    }
}
