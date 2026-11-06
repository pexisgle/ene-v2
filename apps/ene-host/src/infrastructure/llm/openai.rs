//! Builtin adapter for the official OpenAI Responses API.

use crate::{
    domain::{Connection, Error, ErrorKind},
    llm::*,
    ports::Result,
};
use rig_core::providers::openai::OpenAIConfig;

pub const ID: &str = "openai";
pub const MODEL: &str = "gpt-6-luna";
pub const ENDPOINT: &str = "https://api.openai.com/v1";
const REVISION: &str = "builtin-openai-v2-model-catalog";
const ENVIRONMENT_VARIABLE: &str = "OPENAI_API_KEY";

pub fn descriptor() -> Descriptor {
    Descriptor {
        destinations: vec!["https://api.openai.com".into()],
        models: vec![Model {
            id: MODEL.into(),
            name: MODEL.into(),
            ..Default::default()
        }],
        capabilities: Capabilities {
            streaming_text: true,
            model_list: true,
            ..Default::default()
        }
        .into(),
        ..super::builtin_descriptor(ID, "OpenAI", ENDPOINT, ENVIRONMENT_VARIABLE, REVISION)
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
    fn models(&self, input: Invocation) -> futures_util::future::BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async move {
            let transport = crate::llm::AuthorizedTransport::new(
                std::sync::Arc::new(input),
                CredentialPlacement::Bearer,
            )?;
            let mut models = Vec::new();
            for item in super::catalog_items(transport, ENDPOINT).await? {
                let id = item["id"]
                    .as_str()
                    .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?;
                models.push(Model {
                    id: id.into(),
                    name: id.into(),
                    metadata: ene_rpc::ModelMetadata {
                        source: format!("{ENDPOINT}/models"),
                        scope: "route".into(),
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                });
            }
            validate_models(&models)?;
            Ok(models)
        })
    }
    fn embed(
        &self,
        input: Invocation,
    ) -> futures_util::future::BoxFuture<'_, Result<rig_core::embeddings::EmbeddingResponse>> {
        Box::pin(async move {
            let texts = input.embedding_input()?.clone();
            let endpoint = input.connection().endpoint.trim_end_matches('/').to_owned();
            let transport = crate::llm::AuthorizedTransport::new(
                std::sync::Arc::new(input),
                CredentialPlacement::Bearer,
            )?;
            // Use Rig's OpenAI embedding wire through the same authorized transport
            // as the conversation and tool requests. The host owns only the consent,
            // destination and size checks; encoding/response parsing stay in Rig.
            let client = OpenAIConfig::new("managed-by-host")
                .with_base_url(&endpoint)
                .connect(transport);
            let model = client.embedding(crate::llm::EMBEDDING_MODEL, None);
            model
                .call(texts)
                .await
                .map_err(|_| Error::new(ErrorKind::ProviderFailure))
        })
    }
}

/// The task stream's model on Rig's Responses wire.
pub(crate) fn task_stream(
    outbound: std::sync::Arc<crate::llm::Outbound>,
    mut request: rig_core::completion::CompletionRequest,
) -> futures_util::future::BoxFuture<'static, Result<rig_core::streaming::CompletionStream>> {
    Box::pin(async move {
        let model = request.model.clone().ok_or_else(Error::invalid)?;
        // Rig's Responses wire must see store=false before it encodes
        // history; changing the HTTP body downstream would be too late.
        match request
            .additional_params
            .get_or_insert_with(|| serde_json::json!({}))
        {
            serde_json::Value::Object(params) => {
                params.insert("store".into(), serde_json::Value::Bool(false));
            }
            _ => return Err(Error::invalid()),
        }
        let endpoint = outbound.connection().endpoint.clone();
        let transport = crate::llm::AuthorizedTransport::for_outbound(
            outbound,
            crate::llm::CredentialPlacement::Bearer,
        )?;
        let client = OpenAIConfig::new("managed-by-host")
            .with_base_url(endpoint)
            .connect(transport);
        client
            .responses(model)
            .stream(request)
            .map_err(|_| Error::new(ErrorKind::ProviderFailure))
    })
}

/// The parent conversation's model, connected as a Rig kernel agent's
/// effect handler. The host's transport stays the signing and destination
/// boundary; Rig owns the request encoding, tool calls and streaming.
pub(crate) fn agent_connection(
    outbound: std::sync::Arc<crate::llm::Outbound>,
    model: &str,
) -> Result<rig_ecs::model::Connection> {
    use rig_core::operation::Completion;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_ecs::effects::Handler;

    let connection = outbound.connection();
    let endpoint = connection.endpoint.clone();
    let (spec, reference) = crate::infrastructure::llm::tool_spec(connection, ID, model)?;
    let transport = crate::llm::AuthorizedTransport::for_outbound(
        outbound,
        crate::llm::CredentialPlacement::Bearer,
    )?;
    let client = OpenAIConfig::new("managed-by-host")
        .with_base_url(&endpoint)
        .connect(transport);
    // The Responses wire is what this adapter has always spoken.
    let model = client.responses(model.to_owned());
    let handler = ErasedHandler::new(ModelAdapter::<Completion>::new(reference, model));
    Ok(rig_ecs::model::Connection {
        spec,
        handler: Handler(handler),
    })
}
