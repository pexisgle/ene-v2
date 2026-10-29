//! Public model facts; never receives credentials, prompts, or connection URLs.
use crate::{
    domain::{Error, ErrorKind},
    llm::Invocation,
    ports::Result,
};
use ene_rpc::{Model, ModelMetadata, StringList};
use serde_json::Value;

pub const ORIGIN: &str = "https://models.dev";
pub const URL: &str = "https://models.dev/api.json";

pub async fn enrich(models: &mut [Model], input: std::sync::Arc<Invocation>) -> Result<()> {
    optional_enrich(models, input, URL).await
}

async fn optional_enrich(
    models: &mut [Model],
    input: std::sync::Arc<Invocation>,
    source: &str,
) -> Result<()> {
    let result = enrich_from(models, input.clone(), source).await;
    // A missing public catalog must not discard the authenticated provider list.
    // Recheck even on network failure so revocation is never swallowed.
    input.authorization().check().await?;
    match result {
        Err(error) if error.kind == ErrorKind::ExternalFailure => Ok(()),
        result => result,
    }
}

async fn enrich_from(
    models: &mut [Model],
    input: std::sync::Arc<Invocation>,
    source: &str,
) -> Result<()> {
    if models.iter().all(|m| {
        m.metadata.as_option().is_some_and(|info| {
            info.context_window.is_some()
                && info.max_output_tokens.is_some()
                && info.input_modalities.is_set()
                && info.output_modalities.is_set()
        })
    }) {
        return Ok(());
    }
    let failure = || Error::new(ErrorKind::ExternalFailure);
    input.authorization().check().await?;
    let transport = crate::llm::AuthorizedTransport::public_catalog(input)?;
    let request = http::Request::get(source)
        .body(bytes::Bytes::new())
        .map_err(|_| failure())?;
    let response = transport.send(request).await.map_err(|error| {
        if error.kind == ErrorKind::ProviderFailure {
            failure()
        } else {
            error
        }
    })?;
    if !response.status().is_success() {
        return Err(failure());
    }
    let bytes = response.bytes().await.map_err(|error| {
        if error.kind == ErrorKind::ProviderFailure {
            failure()
        } else {
            error
        }
    })?;
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
            input_modalities: strings(&entry["modalities"]["input"])
                .map(|values| StringList {
                    values,
                    ..Default::default()
                })
                .into(),
            output_modalities: strings(&entry["modalities"]["output"])
                .map(|values| StringList {
                    values,
                    ..Default::default()
                })
                .into(),
            ..Default::default()
        };
        if incoming.context_window.is_none()
            && incoming.max_input_tokens.is_none()
            && incoming.max_output_tokens.is_none()
            && incoming.input_modalities.is_unset()
            && incoming.output_modalities.is_unset()
        {
            continue;
        }
        if model.metadata.is_unset() {
            model.metadata = ModelMetadata {
                source: source.into(),
                scope: "catalog".into(),
                ..Default::default()
            }
            .into();
        }
        let info = model.metadata.get_or_insert_default();
        macro_rules! fill {
            ($field:ident) => {
                if info.$field.is_none() && incoming.$field.is_some() {
                    info.$field = incoming.$field;
                    if info.source != source {
                        info.field_sources
                            .insert(stringify!($field).into(), source.into());
                    }
                    info.scope = "catalog".into();
                }
            };
        }
        fill!(context_window);
        fill!(max_input_tokens);
        fill!(max_output_tokens);
        for (field, current, incoming) in [
            (
                "input_modalities",
                &mut info.input_modalities,
                incoming.input_modalities,
            ),
            (
                "output_modalities",
                &mut info.output_modalities,
                incoming.output_modalities,
            ),
        ] {
            if current.is_unset() && incoming.is_set() {
                *current = incoming;
                if info.source != source {
                    info.field_sources.insert(field.into(), source.into());
                }
                info.scope = "catalog".into();
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
