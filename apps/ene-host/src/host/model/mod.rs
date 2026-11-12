//! Built-in LLM providers, capability resolution and authorized connections.
mod credentials;
mod transport;

pub(crate) use self::credentials::{Credentials, secret};

use self::transport::Authorized;
use super::{
    companion::{Companion, ModelAssignment},
    db::{Result, Store},
    error::HostError,
    settings::Connection,
};
use ene_rpc as w;
use rig_core::{
    catalog::{Catalog, ModelSpec},
    operation::Completion,
    providers::{openai::OpenAIConfig, registry::ProviderId},
    serve::{ErasedHandler, adapters::ModelAdapter},
};
use rig_ecs::{effects::Handler, model::Connection as ModelConnection};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

const OPENAI: &str = "openai";
const OPENROUTER: &str = "openrouter";
const MAX_MODELS: usize = 4096;
const MAX_MODEL_ID: usize = 512;
const MAX_PROBE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Purpose {
    Conversation,
    Task,
    Perception,
    Compaction,
    Consolidation,
    Decision,
    Embedding,
}

impl Purpose {
    pub(crate) fn assignment(self, companion: &Companion) -> Option<&ModelAssignment> {
        match self {
            Self::Conversation => companion.assignments.companion.as_ref(),
            Self::Task => companion.assignments.task.as_ref(),
            Self::Perception => companion
                .assignments
                .perception
                .as_ref()
                .or(companion.assignments.companion.as_ref()),
            Self::Compaction => companion
                .assignments
                .compaction
                .as_ref()
                .or(companion.assignments.companion.as_ref()),
            Self::Consolidation => companion
                .assignments
                .consolidation
                .as_ref()
                .or(companion.assignments.companion.as_ref()),
            Self::Decision => companion
                .assignments
                .decision
                .as_ref()
                .or(companion.assignments.task.as_ref()),
            Self::Embedding => companion.assignments.embedding.as_ref(),
        }
    }
}

#[derive(Clone, Debug, bevy_ecs::prelude::Component)]
pub(crate) struct Binding {
    pub(crate) connection: String,
    pub(crate) model: String,
    pub(crate) revision: u64,
    pub(crate) permit: Arc<AtomicBool>,
}

/// Why the last bind attempt failed, so Gate can say more than "unavailable".
#[derive(Clone, Debug, bevy_ecs::prelude::Component)]
pub(crate) struct Refusal(pub(crate) String);

pub(crate) fn revoke(binding: &Binding) {
    binding.permit.store(false, Ordering::Release);
}

pub(crate) fn unchanged(
    store: &Store,
    companion_id: &str,
    purpose: Purpose,
    binding: &Binding,
) -> Result<bool> {
    let companion = store
        .companion(companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let Some(assignment) = purpose.assignment(&companion) else {
        return Ok(false);
    };
    Ok(assignment.connection == binding.connection
        && assignment.model == binding.model
        && store.settings_revision()? == binding.revision)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Responses,
    ChatCompletions,
}

pub(crate) struct Provider {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) endpoint: &'static str,
    pub(crate) environment: &'static str,
    pub(crate) revision: &'static str,
    wire: Wire,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        id: OPENAI,
        name: "OpenAI",
        kind: "llm",
        endpoint: "https://api.openai.com/v1",
        environment: "OPENAI_API_KEY",
        revision: "builtin-openai-v1",
        wire: Wire::Responses,
    },
    Provider {
        id: OPENROUTER,
        name: "OpenRouter",
        kind: "llm",
        endpoint: "https://openrouter.ai/api/v1",
        environment: "OPENROUTER_API_KEY",
        revision: "builtin-openrouter-v1",
        wire: Wire::ChatCompletions,
    },
];

pub(crate) fn builtin(adapter: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|provider| provider.id == adapter)
}

pub(crate) fn descriptors() -> Vec<w::Provider> {
    PROVIDERS
        .iter()
        .map(|provider| w::Provider {
            id: provider.id.into(),
            name: provider.name.into(),
            kind: provider.kind.into(),
            endpoint: provider.endpoint.into(),
            options: "{}".into(),
            requires_credentials: true,
            environment_variable: Some(provider.environment.into()),
            ..Default::default()
        })
        .collect()
}

pub(crate) fn origin(endpoint: &str) -> Option<String> {
    let url = url::Url::parse(endpoint).ok()?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(host)) => host == "localhost",
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return None;
    }
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{}{}",
        url.scheme(),
        match url.host()? {
            url::Host::Ipv6(ip) => format!("[{ip}]"),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Domain(host) => host.to_owned(),
        },
        if url.port().is_some() {
            format!(":{port}")
        } else {
            String::new()
        }
    ))
}

pub(crate) fn validate_connection(
    adapter: &str,
    kind: &str,
    endpoint: &str,
    options: &Value,
) -> Result<()> {
    let provider =
        builtin(adapter).ok_or_else(|| HostError::invalid("unsupported provider adapter"))?;
    if kind != provider.kind {
        return Err(HostError::invalid("unsupported connection kind"));
    }
    if origin(endpoint).is_none() {
        return Err(HostError::invalid(
            "endpoint requires HTTPS or loopback HTTP without credentials, query or fragment",
        ));
    }
    explicit_tool_calling(options)?;
    Ok(())
}

fn explicit_tool_calling(options: &Value) -> Result<Option<bool>> {
    let object = options
        .as_object()
        .ok_or_else(|| HostError::invalid("connection options must be an object"))?;
    let mut tool_calling = None;
    for (key, value) in object {
        if key != "tool_calling" {
            return Err(HostError::invalid("unsupported connection option"));
        }
        tool_calling = Some(
            value
                .as_bool()
                .ok_or_else(|| HostError::invalid("tool_calling must be boolean"))?,
        );
    }
    Ok(tool_calling)
}

fn spec(connection: &Connection, adapter: &str, model: &str) -> Result<Arc<ModelSpec>> {
    if model.trim().is_empty() || model.len() > MAX_MODEL_ID {
        return Err(HostError::invalid("model is required"));
    }
    let reference = format!("{adapter}/{model}");
    let spec = match Catalog::builtin().resolve(&reference) {
        Ok(found) => found.shared(),
        Err(_) => match explicit_tool_calling(&connection.options)? {
            Some(true) => {
                let provider = ProviderId::catalog(adapter)
                    .ok_or_else(|| HostError::invalid("provider is not in the Rig catalog"))?;
                Arc::new(ModelSpec::new(provider, model).with_tools(true))
            }
            Some(false) => return Err(HostError::conflict("model_cannot_call_tools")),
            None => return Err(HostError::unavailable("model_capability_unknown")),
        },
    };
    if !spec.tools {
        return Err(HostError::conflict("model_cannot_call_tools"));
    }
    Ok(spec)
}

pub(crate) trait ModelSource: Send + Sync {
    fn connect(
        &self,
        store: &Store,
        companion_id: &str,
        purpose: Purpose,
    ) -> Result<(ModelConnection, Binding)>;
}

pub(crate) struct Providers {
    pub(crate) credentials: Credentials,
}

impl ModelSource for Providers {
    fn connect(
        &self,
        store: &Store,
        companion_id: &str,
        purpose: Purpose,
    ) -> Result<(ModelConnection, Binding)> {
        let companion = store
            .companion(companion_id)?
            .ok_or_else(|| HostError::not_found("companion not found"))?;
        connect(store, &self.credentials, &companion, purpose)
    }
}

pub(crate) fn connect(
    store: &Store,
    credentials: &Credentials,
    companion: &Companion,
    purpose: Purpose,
) -> Result<(ModelConnection, Binding)> {
    let assignment = purpose
        .assignment(companion)
        .ok_or_else(|| HostError::conflict("model_unset"))?;
    if assignment.connection.is_empty() || assignment.model.is_empty() {
        return Err(HostError::conflict("model_unset"));
    }
    let record = store
        .connection(&assignment.connection)?
        .ok_or_else(|| HostError::conflict("model_unset"))?;
    if !record.enabled {
        return Err(HostError::permission("connection is disabled"));
    }
    let provider = builtin(&record.adapter)
        .ok_or_else(|| HostError::conflict("unsupported provider adapter"))?;
    let destination = origin(&record.endpoint)
        .ok_or_else(|| HostError::invalid("connection endpoint is invalid"))?;
    let revision = store.settings_revision()?;
    let secret =
        secret(credentials, &record).ok_or_else(|| HostError::permission("credential_missing"))?;
    let spec = spec(&record, &record.adapter, &assignment.model)?;
    let permit = Arc::new(AtomicBool::new(true));
    let transport = Authorized::new(permit.clone(), destination)?;
    let handler = match provider.wire {
        Wire::ChatCompletions => {
            let client =
                OpenAIConfig::with_key(&rig_core::providers::openai::wire::OPENROUTER, secret)
                    .with_base_url(&record.endpoint)
                    .connect(transport);
            ErasedHandler::new(ModelAdapter::<Completion>::new(
                spec.reference(),
                client.chat(assignment.model.clone()),
            ))
        }
        Wire::Responses => {
            let client = OpenAIConfig::new(secret)
                .with_base_url(&record.endpoint)
                .connect(transport);
            ErasedHandler::new(ModelAdapter::<Completion>::new(
                spec.reference(),
                client.responses(assignment.model.clone()),
            ))
        }
    };
    Ok((
        ModelConnection {
            spec,
            handler: Handler(handler),
        },
        Binding {
            connection: assignment.connection.clone(),
            model: assignment.model.clone(),
            revision,
            permit,
        },
    ))
}

pub(crate) async fn probe(record: &Connection, secret: &str) -> Result<(Vec<w::Model>, Value)> {
    let provider = builtin(&record.adapter)
        .ok_or_else(|| HostError::unavailable("unsupported provider adapter"))?;
    let endpoint = record.endpoint.trim_end_matches('/');
    let mut response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| HostError::unavailable("provider is unavailable"))?
        .get(format!("{endpoint}/models"))
        .bearer_auth(secret)
        .send()
        .await
        .map_err(|_| HostError::unavailable("provider is unavailable"))?;
    if !response.status().is_success() {
        return Err(HostError::unavailable("provider is unavailable"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROBE_BYTES as u64)
    {
        return Err(HostError::unavailable("provider reply is too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| HostError::unavailable("provider is unavailable"))?
    {
        if bytes.len() + chunk.len() > MAX_PROBE_BYTES {
            return Err(HostError::unavailable("provider reply is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| HostError::unavailable("provider reply is invalid"))?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| HostError::unavailable("provider reply is invalid"))?;
    let models: Vec<w::Model> = items
        .iter()
        .take(MAX_MODELS)
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            (!id.is_empty() && id.len() <= MAX_MODEL_ID).then(|| w::Model {
                id: id.into(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .into(),
                ..Default::default()
            })
        })
        .collect();
    let capabilities = json!({
        "provider": provider.id,
        "revision": provider.revision,
        "model_count": models.len(),
    });
    Ok((models, capabilities))
}
