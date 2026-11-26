//! Add publicly listed model capabilities missing from the bundled catalog.

use crate::host::{agent, background::AbortOnDropHandle, net};
use bevy_ecs::prelude::*;
use parking_lot::RwLock;
use rig_core::{
    catalog::{Catalog, ModelSpec},
    providers::registry::ProviderId,
};
use rig_ecs::model::Models;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

const URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Resource)]
pub(crate) struct Refresh {
    published: Arc<RwLock<Catalog>>,
    pending: watch::Receiver<Catalog>,
    _fetch: Option<AbortOnDropHandle<()>>,
}

impl Refresh {
    pub(crate) async fn initial(
        wake: crossbeam_channel::Sender<()>,
    ) -> (Arc<RwLock<Catalog>>, Self) {
        let (catalog, delay) = if cfg!(test) {
            (Catalog::builtin().clone(), None)
        } else {
            let catalog = fetch().await;
            let delay = Some(next_delay(catalog.is_some()));
            (catalog.unwrap_or_else(|| Catalog::builtin().clone()), delay)
        };
        let (updates, pending) = watch::channel(catalog.clone());
        let published = Arc::new(RwLock::new(catalog));
        let fetch = delay
            .map(|delay| AbortOnDropHandle::new(tokio::spawn(fetch_updates(updates, wake, delay))));
        (
            Arc::clone(&published),
            Self {
                published,
                pending,
                _fetch: fetch,
            },
        )
    }
}

async fn fetch_updates(
    updates: watch::Sender<Catalog>,
    wake: crossbeam_channel::Sender<()>,
    mut delay: Duration,
) {
    loop {
        tokio::time::sleep(delay).await;
        let catalog = fetch().await;
        delay = next_delay(catalog.is_some());
        if let Some(catalog) = catalog {
            if updates.send(catalog).is_err() {
                return;
            }
            let _pending_wake = wake.try_send(());
        }
    }
}

pub(crate) fn apply_refresh(
    mut refresh: ResMut<Refresh>,
    mut models: ResMut<Models>,
    mut connections: ResMut<agent::Connections>,
) {
    if matches!(refresh.pending.has_changed(), Ok(true)) {
        let catalog = refresh.pending.borrow_and_update().clone();
        *models.0.catalog_mut() = catalog.clone();
        connections.clear_models();
        *refresh.published.write() = catalog;
    }
}

fn next_delay(succeeded: bool) -> Duration {
    if succeeded {
        REFRESH_INTERVAL
    } else {
        RETRY_INTERVAL
    }
}

async fn fetch() -> Option<Catalog> {
    match download().await {
        Ok(catalog) => Some(catalog),
        Err(error) => {
            tracing::debug!(%error, "could not refresh model catalog");
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CatalogError {
    #[error(transparent)]
    Net(#[from] net::NetError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
    #[error(transparent)]
    Parse(#[from] rig_core::catalog::CatalogError),
    #[error("models.dev returned no model entries")]
    EmptyCatalog,
}

async fn download() -> Result<Catalog, CatalogError> {
    let response = net::client()?
        .get(URL)
        .header(
            reqwest::header::USER_AGENT,
            concat!("ene-host/", env!("CARGO_PKG_VERSION")),
        )
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)?;
    let body = net::read_response_body(response, 16 * 1024 * 1024, "models.dev catalog").await?;
    let json = std::str::from_utf8(&body)?;
    supplement(Catalog::builtin(), json)
}

fn supplement(base: &Catalog, json: &str) -> Result<Catalog, CatalogError> {
    let (fetched, _) = Catalog::from_models_dev(json)?;
    if fetched.iter().next().is_none() {
        return Err(CatalogError::EmptyCatalog);
    }
    let mut catalog = base.clone();
    for row in fetched.iter() {
        let Some(provider) = ProviderId::catalog(row.provider.vendor()) else {
            continue;
        };
        if base.get_exact(provider, &row.id).is_some() {
            continue;
        }
        // Import facts only. Provider URLs and wire overrides come from rig and local settings.
        let mut spec = ModelSpec::new(provider, &row.id).with_tools(row.tools);
        spec.display_name = row.display_name.clone();
        spec.input = row.input;
        spec.details.output = row.details.output;
        catalog.insert(spec);
    }
    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::catalog::Connector;

    #[test]
    fn ready_catalog_is_published_when_the_agent_connector_is_updated() {
        let catalog = Catalog::default();
        let (updates, pending) = watch::channel(catalog.clone());
        let published = Arc::new(RwLock::new(catalog.clone()));
        let mut world = World::new();
        world.insert_resource(Refresh {
            published: Arc::clone(&published),
            pending,
            _fetch: None,
        });
        world.insert_resource(Models(Connector::new(catalog)));
        world.init_resource::<agent::Connections>();

        let provider = ProviderId::catalog("openai").unwrap();
        let mut updated = Catalog::default();
        updated.insert(ModelSpec::new(provider, "new-model"));
        updates.send(updated).unwrap();
        assert!(published.read().get_exact(provider, "new-model").is_none());

        world.run_system_cached(apply_refresh).unwrap();

        assert!(published.read().get_exact(provider, "new-model").is_some());
        assert!(
            world
                .resource::<Models>()
                .0
                .catalog()
                .get_exact(provider, "new-model")
                .is_some()
        );
    }

    #[test]
    fn looks_up_new_capabilities_without_accepting_remote_routing_or_overriding_bundled_models() {
        let base = Catalog::builtin();
        let catalog = supplement(
            base,
            r#"{"openai":{"models":{
            "ene-new-vision": {
                "name":"New Vision","tool_call":true,
                "modalities":{"input":["text","image"],"output":["text"]},
                "provider":{"api":"https://example.invalid/v1","npm":"@ai-sdk/anthropic"}
            },
            "gpt-4o-mini":{"tool_call":false,"modalities":{"input":["text"]}}
        }}}"#,
        )
        .unwrap();
        let spec = catalog.resolve("openai/ene-new-vision").unwrap().spec;
        let provider = ProviderId::catalog("openai").unwrap();
        assert_eq!(spec.provider, provider);
        assert!(spec.details.provider.is_none());
        assert!(spec.tools && spec.input.image);
        assert!(
            catalog
                .resolve("openai/gpt-4o-mini")
                .unwrap()
                .spec
                .input
                .image
        );
        assert!(supplement(base, "{}").is_err());
    }
}
