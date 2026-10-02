//! Public model facts; never receives credentials, prompts, or connection URLs.
use ene_api_types::provider::{MetadataScope, Model, ModelMetadata};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Result, llm::Authorization},
};
use serde_json::Value;

pub const ORIGIN: &str = "https://models.dev";
pub const URL: &str = "https://models.dev/api.json";
const MAX_BYTES: usize = 16 * 1024 * 1024;

pub async fn enrich(models: &mut [Model], authorization: &dyn Authorization) -> Result<()> {
    enrich_from(models, authorization, URL).await
}

async fn enrich_from(
    models: &mut [Model],
    authorization: &dyn Authorization,
    source: &str,
) -> Result<()> {
    if models.iter().all(|m| {
        m.metadata.as_ref().is_some_and(|info| {
            info.context_window.is_some()
                && info.max_output_tokens.is_some()
                && info.input_modalities.is_some()
                && info.output_modalities.is_some()
        })
    }) {
        return Ok(());
    }
    let failure = || Error::new(ErrorKind::ExternalFailure);
    authorization.check().await?;
    // A separate request with no auth, cookies, body, model ID, or user-selected URL.
    let mut response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ene/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| failure())?
        .get(source)
        .send()
        .await
        .map_err(|_| failure())?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > MAX_BYTES as u64)
    {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| failure())? {
        authorization.check().await?;
        if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
            return Err(failure());
        }
        bytes.extend_from_slice(&chunk);
    }
    authorization.check().await?;
    let catalog: Value = serde_json::from_slice(&bytes).map_err(|_| failure())?;
    apply(models, &catalog, source)
}

fn apply(models: &mut [Model], catalog: &Value, source: &str) -> Result<()> {
    let entries = catalog[super::ID]["models"]
        .as_object()
        .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?;
    for model in models {
        let Some(entry) = entries.get(&model.id) else {
            continue;
        };
        if entry["id"].as_str().is_some_and(|id| id != model.id) {
            continue;
        }
        let positive = |v: &Value| v.as_u64().filter(|v| *v > 0);
        let strings = |v: &Value| -> Option<Vec<String>> {
            v.as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let incoming = ModelMetadata {
            source: source.into(),
            context_window: positive(&entry["limit"]["context"]),
            max_input_tokens: positive(&entry["limit"]["input"]),
            max_output_tokens: positive(&entry["limit"]["output"]),
            input_modalities: strings(&entry["modalities"]["input"]),
            output_modalities: strings(&entry["modalities"]["output"]),
            ..Default::default()
        };
        if incoming.context_window.is_none()
            && incoming.max_input_tokens.is_none()
            && incoming.max_output_tokens.is_none()
            && incoming.input_modalities.is_none()
            && incoming.output_modalities.is_none()
        {
            continue;
        }
        let info = model.metadata.get_or_insert_with(|| ModelMetadata {
            source: source.into(),
            ..Default::default()
        });
        macro_rules! fill {
            ($field:ident) => {
                if info.$field.is_none() && incoming.$field.is_some() {
                    info.$field = incoming.$field;
                    if info.source != source {
                        info.field_sources
                            .insert(stringify!($field).into(), source.into());
                    }
                    info.scope = MetadataScope::Catalog;
                }
            };
        }
        fill!(context_window);
        fill!(max_input_tokens);
        fill!(max_output_tokens);
        fill!(input_modalities);
        fill!(output_modalities);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
