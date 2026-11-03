//! Builtin adapter for the official OpenAI Responses API.
mod catalog;

use crate::{
    domain::{Connection, Error, ErrorKind},
    llm::*,
    ports::Result,
};
use futures_util::{StreamExt, stream::BoxStream};
use rig_core::providers::openai::{OpenAI, OpenAIConfig};
use serde_json::{Value, json};

pub const ID: &str = "openai";
pub const MODEL: &str = "gpt-6-luna";
pub const ENDPOINT: &str = "https://api.openai.com/v1";
const REVISION: &str = "builtin-openai-v2-model-catalog";

pub fn descriptor() -> Descriptor {
    Descriptor {
        id: ID.into(),
        name: "OpenAI".into(),
        kind: "builtin".into(),
        endpoint: ENDPOINT.into(),
        options: ene_rpc::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        settings: vec![],
        requires_credentials: true,
        environment_variable: Some("OPENAI_API_KEY".into()),
        destinations: vec!["https://api.openai.com".into(), catalog::ORIGIN.into()],
        state: "ready".into(),
        error: None,
        revision: REVISION.into(),
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
        ..Default::default()
    }
}

pub fn policy(c: &Connection) -> Result<Policy> {
    if c.kind != "llm"
        || c.adapter != ID
        || c.endpoint != ENDPOINT
        || crate::infrastructure::llm::explicit_tool_calling(c).is_err()
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    Ok(Policy {
        revision: REVISION.into(),
        fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":serde_json::from_str::<Value>(&c.options.json).unwrap_or(Value::Null),"implementation":REVISION}),
        destinations: descriptor().destinations,
        requires_credentials: true,
        environment_variable: Some("OPENAI_API_KEY".into()),
    })
}

pub async fn generate(input: Invocation) -> Result<BoxStream<'static, Result<Event>>> {
    let request = input.generation()?.clone();
    let transport = crate::llm::AuthorizedTransport::new(
        std::sync::Arc::new(input),
        CredentialPlacement::Bearer,
    )?;
    // The SDK sees a placeholder; only the transport can attach real credentials.
    let client = OpenAIConfig::new("managed-by-host")
        .with_base_url(ENDPOINT)
        .connect(transport);
    stream(request, client)
}

fn stream(input: Generation, client: OpenAI) -> Result<BoxStream<'static, Result<Event>>> {
    use rig_core::{
        completion::CompletionRequest,
        streaming::{Item, StreamEvent},
    };
    let mut all = input.messages().to_vec();
    let prompt = all.pop().ok_or_else(Error::invalid)?.text;
    let history = all.into_iter().map(|m| {
        if m.role == crate::llm::ChatRole::User {
            rig_core::message::Message::user(m.text)
        } else {
            rig_core::message::Message::assistant(m.text)
        }
    });
    let request = CompletionRequest::new(prompt)
        .preamble(input.preamble().to_owned())
        .messages(history)
        .max_tokens(input.max_tokens())
        .additional_params(json!({"store":false}));
    let mut stream = client
        .responses(input.model().to_owned())
        .stream(request)
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    Ok(Box::pin(async_stream::try_stream! {
        while let Some(item) = stream.next().await {
            if let Item::Event(StreamEvent::Text { text, .. }) = item.map_err(|_| Error::new(ErrorKind::ExternalFailure))? {
                yield Event::Delta(text);
            }
        }
        let response = stream.finish().await.map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
        yield Event::Finished(if matches!(response.finish_reason(), Some(rig_core::completion::FinishReason::Stop)) {
            "stop"
        } else { "incomplete" }.into());
    }))
}

pub async fn models(input: Invocation) -> Result<Vec<Model>> {
    let input = std::sync::Arc::new(input);
    let transport =
        crate::llm::AuthorizedTransport::new(input.clone(), CredentialPlacement::Bearer)?;
    let mut models = fetch_models(transport, ENDPOINT).await?;
    validate_models(&models)?;
    catalog::enrich(&mut models, input).await?;
    Ok(models)
}

async fn fetch_models(
    transport: crate::llm::AuthorizedTransport,
    endpoint: &str,
) -> Result<Vec<Model>> {
    let request = http::Request::get(format!("{endpoint}/models"))
        .body(bytes::Bytes::new())
        .map_err(|_| Error::invalid())?;
    let response = transport.send(request).await?;
    if !response.status().is_success() {
        return Err(Error::new(ErrorKind::ProviderFailure));
    }
    let value: Value = serde_json::from_slice(&response.bytes().await?)
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    value["data"]
        .as_array()
        .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?
        .iter()
        .map(|v| {
            let id = v["id"]
                .as_str()
                .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?;
            Ok(Model {
                id: id.into(),
                name: id.into(),
                metadata: ene_rpc::ModelMetadata {
                    source: format!("{endpoint}/models"),
                    scope: "route".into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
        })
        .collect()
}

pub struct Provider;
impl LlmProvider for Provider {
    fn policy(&self, c: &Connection) -> Result<Policy> {
        policy(c)
    }
    fn generate(
        &self,
        i: Invocation,
    ) -> futures_util::future::BoxFuture<'_, Result<BoxStream<'static, Result<Event>>>> {
        Box::pin(generate(i))
    }
    fn models(&self, i: Invocation) -> futures_util::future::BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(models(i))
    }
}

/// The parent conversation's model, connected as a Rig kernel agent's
/// effect handler. The host's transport stays the signing and destination
/// boundary; Rig owns the request encoding, tool calls and streaming.
pub(crate) fn agent_connection(
    outbound: std::sync::Arc<crate::llm::Outbound>,
    model: &str,
) -> Result<rig_ecs::model::Connection> {
    use rig_core::catalog::{Catalog, ModelSpec};
    use rig_core::operation::Completion;
    use rig_core::providers::registry::ProviderId;
    use rig_core::serve::ErasedHandler;
    use rig_core::serve::adapters::ModelAdapter;
    use rig_ecs::effects::Handler;

    let connection = outbound.connection();
    let endpoint = connection.endpoint.clone();
    let reference = format!("{ID}/{model}");
    let spec = match Catalog::builtin().resolve(&reference) {
        Ok(resolved) => resolved.shared(),
        Err(_) => match crate::infrastructure::llm::explicit_tool_calling(connection)? {
            Some(true) => {
                let provider = ProviderId::catalog(ID)
                    .ok_or_else(|| Error::new(ErrorKind::UnsupportedCapability))?;
                std::sync::Arc::new(ModelSpec::new(provider, model).with_tools(true))
            }
            Some(false) => {
                return Err(Error {
                    kind: ErrorKind::UnsupportedCapability,
                    reason: Some("model_cannot_call_tools"),
                });
            }
            None => {
                return Err(Error {
                    kind: ErrorKind::UnsupportedCapability,
                    reason: Some("model_capability_unknown"),
                });
            }
        },
    };
    if !spec.tools {
        return Err(Error {
            kind: ErrorKind::UnsupportedCapability,
            reason: Some("model_cannot_call_tools"),
        });
    }
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
