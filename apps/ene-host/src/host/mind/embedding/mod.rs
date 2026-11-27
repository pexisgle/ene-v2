//! Embeddings: the connection that generates them, the f32 LE BLOBs SQLite
//! keeps as the canonical copy, and the flat in-memory index recall scans.
//!
//! The database stays canonical: every vector is written back through
//! the backfill writer into its own space. Whenever the content revision
//! changes, SQLite deletes the old vectors so the new text is re-embedded
//! (docs/design/mind.md §4.1).
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
use ene_mind::{RecallCandidate, RecallMode};
use parking_lot::Mutex;
use rig_core::{driver::DynModel, operation::Embedding, providers::registry::ProviderConfig};
use rig_ecs::prelude::*;
use std::{collections::HashMap, sync::Arc, time::Duration};

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
    plugin_revision: u64,
    credential_generation: Option<u64>,
    targets: HashMap<(ene_domain::ConnectionId, String), Target>,
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

struct ConnectParams<'a> {
    connection: &'a Connection,
    credentials: Option<&'a Credentials>,
    endpoint: &'a str,
    model_id: &'a str,
    ndims: Option<usize>,
    plugin_ndims: usize,
    providers: &'a crate::host::providers::Providers,
}

fn connect_with_ndims(params: &ConnectParams<'_>) -> Result<DynModel<Embedding>> {
    let ConnectParams {
        connection,
        credentials,
        endpoint,
        model_id,
        ndims,
        plugin_ndims,
        providers,
    } = *params;
    let adapter = connection.endpoint().adapter.as_str();
    // A plugin provider answers through the host's broker, not through rig's
    // HTTP client, so it is resolved before the catalog is consulted.
    if let Some(running) = providers.bound_running(adapter, ene_plugin::Extension::Embedding)? {
        let secret = crate::host::model::connection_secret(connection, credentials, providers)?;
        let context = crate::host::providers::ProviderInvocationContext::from_connection(
            &running,
            connection,
            secret,
            providers,
            Some(EMBED_TIMEOUT),
        )?;
        let vendor = running
            .provider()
            .ok_or_else(|| {
                HostError::Unavailable(("the extension serves no provider to call").into())
            })?
            .id
            .to_string();
        return Ok(super::plugin::embedding(
            running,
            vendor,
            context,
            model_id.to_owned(),
            plugin_ndims,
        ));
    }
    let secret = crate::host::model::connection_secret(connection, credentials, providers)?;
    let config = crate::host::model::provider_config(adapter, endpoint, secret, providers);
    let http = rig_reqwest::ReqwestClient::from(
        net::client().map_err(|error| HostError::Unavailable(error.to_string()))?,
    );
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
    connect_with_ndims(&ConnectParams {
        connection,
        credentials,
        endpoint,
        model_id,
        ndims: Some(dimensions),
        plugin_ndims: dimensions,
        providers,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EmbeddingProbeError {
    #[error("埋め込み接続ではありません")]
    NotEmbeddingConnection,
    #[error("接続先URLが設定されていません")]
    MissingEndpoint,
    #[error("埋め込みエンドポイントがタイムアウトしました")]
    Timeout,
    #[error("埋め込みエンドポイントがベクトルを返しませんでした")]
    EmptyResponse,
    #[error("{0}")]
    Failed(String),
}

/// Probes an embedding connection by generating one sample vector without forcing
/// `dimensions` on the wire so the host observes the server's actual output width.
pub(crate) async fn probe_dimensions(
    connection: &Connection,
    credentials: &Credentials,
    providers: &crate::host::providers::Providers,
) -> std::result::Result<usize, EmbeddingProbeError> {
    let ConnectionConfig::Embedding(config) = &connection.config else {
        return Err(EmbeddingProbeError::NotEmbeddingConnection);
    };
    let base = crate::host::settings::endpoint_of(connection, providers)
        .ok_or(EmbeddingProbeError::MissingEndpoint)?;
    let adapter = connection.endpoint().adapter.as_str();
    if providers.claims(adapter, ene_plugin::Extension::Embedding) {
        providers
            .start(adapter, ene_plugin::Extension::Embedding)
            .await
            .map_err(|error| EmbeddingProbeError::Failed(error.to_string()))?;
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
    let model = connect_with_ndims(&ConnectParams {
        connection,
        credentials: Some(credentials),
        endpoint: &base,
        model_id,
        ndims: None,
        plugin_ndims: config.dimensions as usize,
        providers,
    })
    .map_err(|error| EmbeddingProbeError::Failed(error.to_string()))?;
    let response = tokio::time::timeout(EMBED_TIMEOUT, model.call(vec!["ping".to_owned()]))
        .await
        .map_err(|_| EmbeddingProbeError::Timeout)?
        .map_err(|error| EmbeddingProbeError::Failed(error.to_string()))?;
    let first = response
        .embeddings
        .into_iter()
        .next()
        .ok_or(EmbeddingProbeError::EmptyResponse)?;
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
    let Some(connection) = store
        .settings_repo()
        .connection(&assignment.connection_id)?
    else {
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
        ene_catalog::known_embedding_dimensions(model_id).unwrap_or(0) as usize
    };
    // The space id is only stable when the dimensions are known up front.
    if model_id.is_empty() || dimensions == 0 || dimensions > MAX_DIMENSIONS {
        return Ok(None);
    }
    let Some(endpoint) = crate::host::settings::endpoint_of(&connection, &embeddings.providers)
    else {
        return Ok(None);
    };
    let revision = store.settings_repo().settings_revision()?;
    let key = (connection.id.clone(), model_id.to_owned());
    let mut models = embeddings.models.lock();
    let credential_generation = credentials.map(Credentials::generation);
    let plugin_revision = embeddings.providers.revision();
    if models.revision != revision
        || models.plugin_revision != plugin_revision
        || models.credential_generation != credential_generation
    {
        models.targets.clear();
        models.revision = revision;
        models.plugin_revision = plugin_revision;
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

pub(crate) async fn semantic_candidates(
    store: &Store,
    embeddings: &Embeddings,
    credentials: Option<&Credentials>,
    companion: &str,
    query: &str,
    mode: RecallMode,
) -> Vec<RecallCandidate> {
    let Ok(Some(record)) = store.companion_repo().companion(companion) else {
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
    let mut index = embeddings.index.lock();
    if index.sync(store, companion, &target.space).is_err() {
        return Vec::new();
    }
    index.ranked(companion, &target.space, &vector, mode, 20)
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
mod tests;
