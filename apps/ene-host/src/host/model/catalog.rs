//! Add publicly listed model capabilities missing from the bundled catalog.

use crate::host::{agent, background, net, resources::HostRuntime};
use bevy_ecs::prelude::*;
use rig_core::{
    catalog::{Catalog, Connector, ModelSpec},
    providers::registry::ProviderId,
};
use rig_ecs::{model::Models, prelude::Wake};
use std::time::{Duration, Instant};

const URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Resource)]
pub(crate) struct Refresh {
    next: Option<Instant>,
    task: Option<background::AbortOnDropHandle<Option<Catalog>>>,
}

impl Refresh {
    pub(crate) async fn initial() -> (Models, Self) {
        let (catalog, next) = if cfg!(test) {
            (None, None)
        } else {
            let catalog = fetch().await;
            let next = Some(next_attempt(catalog.is_some()));
            (catalog, next)
        };
        let models = Models(Connector::new(
            catalog.unwrap_or_else(|| Catalog::builtin().clone()),
        ));
        (models, Self { next, task: None })
    }
}

pub(crate) fn refresh(world: &mut World) {
    let finished = world
        .resource_mut::<Refresh>()
        .task
        .take_if(|task| task.is_finished());
    if let Some(task) = finished {
        let updated = background::take(task).flatten();
        world.resource_mut::<Refresh>().next = Some(next_attempt(updated.is_some()));
        if let Some(catalog) = updated {
            *world.resource_mut::<Models>().0.catalog_mut() = catalog;
            agent::clear_model_cache(world);
        }
        return;
    }
    let state = world.resource::<Refresh>();
    if state.task.is_some() || !state.next.is_some_and(|next| next <= Instant::now()) {
        return;
    }
    let runtime = world.resource::<HostRuntime>().0.clone();
    let wake = world.get_resource::<Wake>().cloned();
    world.resource_mut::<Refresh>().task =
        Some(background::spawn(&runtime, wake.as_ref(), fetch()));
}

fn next_attempt(succeeded: bool) -> Instant {
    Instant::now()
        + if succeeded {
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

async fn download() -> Result<Catalog, String> {
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
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| error.to_string())?;
    let body = net::read_response_body(response, 16 * 1024 * 1024, "models.dev catalog").await?;
    let json = std::str::from_utf8(&body).map_err(|error| error.to_string())?;
    supplement(Catalog::builtin(), json)
}

fn supplement(base: &Catalog, json: &str) -> Result<Catalog, String> {
    let (fetched, _) = Catalog::from_models_dev(json).map_err(|error| error.to_string())?;
    if fetched.iter().next().is_none() {
        return Err("models.dev returned no model entries".into());
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
