//! Builtin adapter for OpenRouter over Rig's native OpenAI-compatible chat
//! wire. Route selection and provider credentials stay OpenRouter's own; they
//! are not reused from the OpenAI connection.

use crate::{
    domain::{Connection, Error, ErrorKind},
    llm::*,
    ports::Result,
};
use futures_util::future::BoxFuture;
use rig_core::operation::Completion;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::providers::openai::wire::OPENROUTER;
use rig_core::serve::ErasedHandler;
use rig_core::serve::adapters::ModelAdapter;
use rig_ecs::effects::Handler;
use std::sync::Arc;

pub const ID: &str = "openrouter";
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1";
const REVISION: &str = "builtin-openrouter-v1-catalog";
const ENVIRONMENT_VARIABLE: &str = "OPENROUTER_API_KEY";
const DESTINATIONS: [&str; 1] = ["https://openrouter.ai"];

pub fn descriptor() -> Descriptor {
    Descriptor {
        destinations: DESTINATIONS.iter().map(|value| (*value).into()).collect(),
        // The model list is read from OpenRouter's own endpoint; the catalog
        // also supplies facts for known models.
        models: vec![],
        capabilities: Capabilities {
            streaming_text: true,
            model_list: true,
            ..Default::default()
        }
        .into(),
        ..super::builtin_descriptor(ID, "OpenRouter", ENDPOINT, ENVIRONMENT_VARIABLE, REVISION)
    }
}

pub fn policy(c: &Connection) -> Result<Policy> {
    super::compatible_policy(
        c,
        ID,
        ENDPOINT,
        REVISION,
        ENVIRONMENT_VARIABLE,
        descriptor().destinations,
    )
}

pub struct Provider;
impl LlmProvider for Provider {
    fn policy(&self, c: &Connection) -> Result<Policy> {
        policy(c)
    }
    fn generate(
        &self,
        _: Invocation,
    ) -> BoxFuture<'_, Result<futures_util::stream::BoxStream<'static, Result<Event>>>> {
        // The parent conversation uses the native Rig connection. One-shot
        // provider calls (connection check and task inference) are not wired
        // to OpenRouter yet and report that explicitly.
        Box::pin(async { Err(Error::new(ErrorKind::UnsupportedCapability)) })
    }
    /// The OpenRouter model list, with tool capability from
    /// `supported_parameters`. The transport still authorizes and signs the GET.
    fn models(&self, input: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async move {
            let endpoint = input.connection().endpoint.trim_end_matches('/').to_owned();
            let transport = crate::llm::AuthorizedTransport::new(
                std::sync::Arc::new(input),
                crate::llm::CredentialPlacement::Bearer,
            )?;
            let mut models = Vec::new();
            for item in super::catalog_items(transport, &endpoint).await? {
                let Some(id) = item["id"].as_str().filter(|id| !id.is_empty()) else {
                    continue;
                };
                let supports_tools =
                    item["supported_parameters"]
                        .as_array()
                        .is_some_and(|parameters| {
                            parameters.iter().any(|parameter| parameter == "tools")
                        });
                models.push(Model {
                    id: id.into(),
                    name: item["name"].as_str().unwrap_or(id).into(),
                    metadata: ene_rpc::ModelMetadata {
                        source: format!("{endpoint}/models"),
                        scope: "route".into(),
                        context_window: item["context_length"].as_u64(),
                        max_output_tokens: item["top_provider"]["max_completion_tokens"].as_u64(),
                        supported_parameters: if supports_tools {
                            ene_rpc::StringList {
                                values: vec!["tools".into()],
                                ..Default::default()
                            }
                            .into()
                        } else {
                            Default::default()
                        },
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                });
            }
            Ok(models)
        })
    }
}

/// The parent conversation's OpenRouter model on Rig's native chat wire.
pub(crate) fn agent_connection(
    outbound: Arc<Outbound>,
    model: &str,
) -> Result<rig_ecs::model::Connection> {
    let connection = outbound.connection();
    let endpoint = connection.endpoint.clone();
    let (spec, reference) = crate::infrastructure::llm::tool_spec(connection, ID, model)?;
    let transport = crate::llm::AuthorizedTransport::for_outbound(
        outbound,
        crate::llm::CredentialPlacement::Bearer,
    )?;
    let client = OpenAIConfig::with_key(&OPENROUTER, "managed-by-host")
        .with_base_url(&endpoint)
        .connect(transport);
    let model = client.chat(model.to_owned());
    let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(reference, model));
    Ok(rig_ecs::model::Connection {
        spec,
        handler: Handler(handler),
    })
}
