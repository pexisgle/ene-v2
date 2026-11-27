//! Add publicly listed model capabilities missing from the bundled catalog.

use crate::host::{agent, background::AbortOnDropHandle, net};
use bevy_ecs::prelude::*;
use rig_core::{
    catalog::{Catalog, ModelSpec},
    providers::registry::ProviderId,
};
use rig_ecs::model::Models;
use std::time::Duration;
use tokio::sync::watch;

const URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Resource)]
pub(crate) struct Refresh {
    pending: watch::Receiver<Catalog>,
    _fetch: Option<AbortOnDropHandle<()>>,
}

impl Refresh {
    pub(crate) fn start(wake: crossbeam_channel::Sender<()>) -> Self {
        let (updates, pending) = watch::channel(Catalog::default());
        let fetch = (!cfg!(test))
            .then(|| AbortOnDropHandle::new(tokio::spawn(fetch_updates(updates, wake))));
        Self {
            pending,
            _fetch: fetch,
        }
    }
}

async fn fetch_updates(updates: watch::Sender<Catalog>, wake: crossbeam_channel::Sender<()>) {
    loop {
        let catalog = fetch().await;
        let delay = if catalog.is_some() {
            REFRESH_INTERVAL
        } else {
            RETRY_INTERVAL
        };
        if let Some(catalog) = catalog {
            if updates.send(catalog).is_err() {
                return;
            }
            let _pending_wake = wake.try_send(());
        }
        tokio::time::sleep(delay).await;
    }
}

pub(crate) fn apply_refresh(
    mut refresh: ResMut<Refresh>,
    mut models: ResMut<Models>,
    mut connections: ResMut<agent::Connections>,
) {
    if matches!(refresh.pending.has_changed(), Ok(true)) {
        *models.0.catalog_mut() = refresh.pending.borrow_and_update().clone();
        connections.clear_models();
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

    #[test]
    fn ready_catalog_is_published_when_the_agent_connector_is_updated() {
        let catalog = Catalog::default();
        let (updates, pending) = watch::channel(catalog);
        let mut world = World::new();
        let refresh = Refresh {
            pending,
            _fetch: None,
        };
        world.init_resource::<Models>();
        world.insert_resource(refresh);
        world.init_resource::<agent::Connections>();

        let provider = ProviderId::catalog("openai").unwrap();
        let mut updated = Catalog::default();
        updated.insert(ModelSpec::new(provider, "new-model"));
        updates.send(updated).unwrap();
        assert!(
            world
                .resource::<Models>()
                .0
                .catalog()
                .get_exact(provider, "new-model")
                .is_none()
        );

        world.run_system_cached(apply_refresh).unwrap();

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
