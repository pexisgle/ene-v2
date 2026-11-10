//! Built-in LLM providers, capability resolution and authorized model connections.
mod credentials;
mod transport;

pub(crate) use self::credentials::{Credentials, secret};

use self::transport::Authorized;
use super::{
    Host,
    db::{Database, Result, aborted, invalid, permission, unavailable},
    mind::{CompanionRecord, companion},
    settings::{ConnectionRecord, GrantRecord, revision as settings_revision},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_core::{
    catalog::{Catalog, ModelSpec},
    operation::Completion,
    providers::{openai::OpenAIConfig, registry::ProviderId},
    serve::{ErasedHandler, adapters::ModelAdapter},
};
use rig_ecs::{effects::Handler, model::Connection as ModelConnection};
use serde_json::Value;
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    Conversation,
    Task,
    Memory,
}

impl Purpose {
    pub(crate) const ALL: [Self; 3] = [Self::Conversation, Self::Task, Self::Memory];

    pub(crate) fn from_str(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == value)
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Task => "task",
            Self::Memory => "memory",
        }
    }

    /// The data kinds a grant for this purpose must cover.
    pub(crate) fn required_data_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Conversation => &["conversation"],
            Self::Task => &["task"],
            Self::Memory => &["memory"],
        }
    }

    fn assignment(self, companion: &CompanionRecord) -> Option<&w::ModelAssignment> {
        let assignments = companion.companion.assignments.as_option()?;
        match self {
            Self::Conversation | Self::Memory => assignments.companion.as_option(),
            Self::Task => assignments.task.as_option(),
        }
    }
}

#[derive(Clone, Component)]
pub(crate) struct Binding {
    pub(crate) connection: String,
    pub(crate) model: String,
    pub(crate) revision: u64,
    pub(crate) permit: Arc<AtomicBool>,
}

/// What the last bind attempt for a call failed with, so the gate can say
/// why instead of "model permission changed".
#[derive(Component, Clone)]
pub(crate) struct Refused(pub(crate) String);

pub(crate) fn revoke(binding: &Binding) {
    binding.permit.store(false, Ordering::Release);
}

/// Whether `binding` is still what [`connect`] would give: the same
/// assignment under the same settings.
///
/// Cheaper than connecting, which lists every grant and builds a transport
/// only to throw it away.
pub(crate) fn unchanged(
    host: &Host,
    companion_id: &str,
    purpose: Purpose,
    binding: &Binding,
) -> Result<bool> {
    let record = companion(&host.db, companion_id)?;
    let Some(assignment) = purpose.assignment(&record) else {
        return Ok(false);
    };
    Ok(assignment.connection_id == binding.connection
        && assignment.model_id == binding.model
        && settings_revision(&host.db)? == binding.revision)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Responses,
    ChatCompletions,
}

pub(crate) struct Provider {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    /// What the connection is for: only `llm` is implemented today.
    pub(crate) kind: &'static str,
    pub(crate) endpoint: &'static str,
    pub(crate) environment: &'static str,
    pub(crate) revision: &'static str,
    pub(crate) default_model: &'static str,
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
        default_model: "gpt-6-luna",
        wire: Wire::Responses,
    },
    Provider {
        id: OPENROUTER,
        name: "OpenRouter",
        kind: "llm",
        endpoint: "https://openrouter.ai/api/v1",
        environment: "OPENROUTER_API_KEY",
        revision: "builtin-openrouter-v1",
        default_model: "google/gemini-2.5-flash-lite",
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
            requires_credentials: true,
            environment_variable: Some(provider.environment.into()),
            destinations: vec![origin(provider.endpoint).unwrap_or_default()],
            state: "ready".into(),
            revision: provider.revision.into(),
            capabilities: w::ProviderCapabilities {
                streaming_text: true,
                model_list: true,
                ..Default::default()
            }
            .into(),
            models: vec![w::Model {
                id: provider.default_model.into(),
                name: provider.default_model.into(),
                ..Default::default()
            }],
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
    let provider = builtin(adapter).ok_or_else(|| invalid("unsupported provider adapter"))?;
    if kind != provider.kind {
        return Err(invalid("unsupported connection kind"));
    }
    if origin(endpoint).is_none() {
        return Err(invalid(
            "endpoint requires HTTPS or loopback HTTP without credentials, query or fragment",
        ));
    }
    explicit_tool_calling(options)?;
    Ok(())
}

fn explicit_tool_calling(options: &Value) -> Result<Option<bool>> {
    let object = options
        .as_object()
        .ok_or_else(|| invalid("connection options must be an object"))?;
    let mut tool_calling = None;
    for (key, value) in object {
        if key != "tool_calling" {
            return Err(invalid("unsupported connection option"));
        }
        tool_calling = Some(
            value
                .as_bool()
                .ok_or_else(|| invalid("tool_calling must be boolean"))?,
        );
    }
    Ok(tool_calling)
}

pub(crate) fn fingerprint(provider: &Provider, connection: &w::Connection) -> Value {
    let options = connection
        .options
        .as_option()
        .and_then(|document| serde_json::from_str::<Value>(&document.json).ok())
        .unwrap_or(Value::Null);
    serde_json::json!({
        "contract": "ene-policy-v1",
        "adapter": connection.adapter,
        "endpoint": connection.endpoint,
        "options": options,
        "implementation": provider.revision,
    })
}

fn spec(connection: &w::Connection, adapter: &str, model: &str) -> Result<Arc<ModelSpec>> {
    if model.trim().is_empty() || model.len() > MAX_MODEL_ID {
        return Err(invalid("model is required"));
    }
    let options = connection
        .options
        .as_option()
        .and_then(|document| serde_json::from_str::<Value>(&document.json).ok())
        .unwrap_or(Value::Null);
    let reference = format!("{adapter}/{model}");
    let spec = match Catalog::builtin().resolve(&reference) {
        Ok(found) => found.shared(),
        Err(_) => match explicit_tool_calling(&options)? {
            Some(true) => {
                let provider = ProviderId::catalog(adapter)
                    .ok_or_else(|| invalid("provider is not in the Rig catalog"))?;
                Arc::new(ModelSpec::new(provider, model).with_tools(true))
            }
            Some(false) => return Err(aborted("model_cannot_call_tools")),
            None => return Err(aborted("model_capability_unknown")),
        },
    };
    if !spec.tools {
        return Err(aborted("model_cannot_call_tools"));
    }
    Ok(spec)
}

/// Where an agent's model comes from: the built-in providers, or a script
/// in a test. Every path that binds an agent goes through it, so a test
/// exercises the same companion checks as the running host.
pub(crate) trait ModelSource: Send + Sync {
    fn connect(
        &self,
        host: &Host,
        companion_id: &str,
        purpose: Purpose,
    ) -> Result<(ModelConnection, Binding)>;
}

pub(crate) struct Providers;

impl ModelSource for Providers {
    fn connect(
        &self,
        host: &Host,
        companion_id: &str,
        purpose: Purpose,
    ) -> Result<(ModelConnection, Binding)> {
        let record = companion(&host.db, companion_id)?;
        connect(&host.db, &host.credentials, &record, purpose)
    }
}

pub(crate) fn connect(
    database: &Database,
    credentials: &Credentials,
    companion: &CompanionRecord,
    purpose: Purpose,
) -> Result<(ModelConnection, Binding)> {
    let assignment = purpose
        .assignment(companion)
        .ok_or_else(|| aborted("model_unset"))?;
    if assignment.connection_id.is_empty() || assignment.model_id.is_empty() {
        return Err(aborted("model_unset"));
    }
    let record: ConnectionRecord = database
        .get(&format!("connection:{}", assignment.connection_id))?
        .ok_or_else(|| aborted("model_unset"))?;
    if !record.connection.enabled {
        return Err(permission("connection is disabled"));
    }
    let provider = builtin(&record.connection.adapter)
        .ok_or_else(|| aborted("unsupported provider adapter"))?;
    let expected = fingerprint(provider, &record.connection);
    let destination = origin(&record.connection.endpoint)
        .ok_or_else(|| invalid("connection endpoint is invalid"))?;
    let revision = settings_revision(database)?;
    let permitted = database
        .list::<GrantRecord>("grant:")?
        .into_iter()
        .any(|(_, g)| {
            g.permits(
                &companion.companion.id,
                &assignment.connection_id,
                purpose.name(),
                &destination,
                &expected,
                revision,
            )
        });
    if !permitted {
        return Err(permission("permission_required"));
    }
    let secret = secret(credentials, &record).ok_or_else(|| permission("credential_missing"))?;
    let spec = spec(
        &record.connection,
        &record.connection.adapter,
        &assignment.model_id,
    )?;
    let permit = Arc::new(AtomicBool::new(true));
    let transport = Authorized::new(permit.clone(), destination)?;
    let handler = match provider.wire {
        Wire::ChatCompletions => {
            let client =
                OpenAIConfig::with_key(&rig_core::providers::openai::wire::OPENROUTER, secret)
                    .with_base_url(&record.connection.endpoint)
                    .connect(transport);
            ErasedHandler::new(ModelAdapter::<Completion>::new(
                spec.reference(),
                client.chat(assignment.model_id.clone()),
            ))
        }
        Wire::Responses => {
            let client = OpenAIConfig::new(secret)
                .with_base_url(&record.connection.endpoint)
                .connect(transport);
            ErasedHandler::new(ModelAdapter::<Completion>::new(
                spec.reference(),
                client.responses(assignment.model_id.clone()),
            ))
        }
    };
    Ok((
        ModelConnection {
            spec,
            handler: Handler(handler),
        },
        Binding {
            connection: assignment.connection_id.clone(),
            model: assignment.model_id.clone(),
            revision,
            permit,
        },
    ))
}

pub(crate) async fn probe(
    record: &ConnectionRecord,
    secret: &str,
) -> Result<(Vec<w::Model>, Value)> {
    let provider = builtin(&record.connection.adapter)
        .ok_or_else(|| unavailable("unsupported provider adapter"))?;
    let endpoint = record.connection.endpoint.trim_end_matches('/');
    let mut response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unavailable("provider is unavailable"))?
        .get(format!("{endpoint}/models"))
        .bearer_auth(secret)
        .send()
        .await
        .map_err(|_| unavailable("provider is unavailable"))?;
    if !response.status().is_success() {
        return Err(unavailable("provider is unavailable"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROBE_BYTES as u64)
    {
        return Err(unavailable("provider reply is too large"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| unavailable("provider is unavailable"))?
    {
        if bytes.len() + chunk.len() > MAX_PROBE_BYTES {
            return Err(unavailable("provider reply is too large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| unavailable("provider reply is invalid"))?;
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| unavailable("provider reply is invalid"))?;
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
    let capabilities = serde_json::json!({
        "provider": provider.id,
        "revision": provider.revision,
        "model_count": models.len(),
    });
    Ok((models, capabilities))
}
