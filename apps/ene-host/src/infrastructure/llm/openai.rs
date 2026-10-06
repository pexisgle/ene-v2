//! Builtin adapter for the official OpenAI Responses API.
mod catalog;

use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
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
        options: json!({}),
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
            metadata: None,
        }],
        capabilities: Capabilities {
            streaming_text: true,
            model_list: true,
        },
    }
}

pub fn policy(c: &Connection) -> Result<Policy> {
    if c.kind != "llm" || c.adapter != ID || c.endpoint != ENDPOINT || c.options != json!({}) {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    Ok(Policy {
        revision: REVISION.into(),
        fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":c.options,"implementation":REVISION}),
        destinations: descriptor().destinations,
        requires_credentials: true,
        environment_variable: Some("OPENAI_API_KEY".into()),
    })
}

pub async fn generate(input: Invocation) -> Result<BoxStream<'static, Result<Event>>> {
    let request = input.generation()?.clone();
    let transport = ene_outbound::AuthorizedTransport::new(
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
        if m.role == ene_core::ports::llm::ChatRole::User {
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
        ene_outbound::AuthorizedTransport::new(input.clone(), CredentialPlacement::Bearer)?;
    let mut models = fetch_models(transport, ENDPOINT).await?;
    validate_models(&models)?;
    catalog::enrich(&mut models, input).await?;
    Ok(models)
}

async fn fetch_models(
    transport: ene_outbound::AuthorizedTransport,
    endpoint: &str,
) -> Result<Vec<Model>> {
    let request = ene_outbound::http::Request::get(format!("{endpoint}/models"))
        .body(ene_outbound::Bytes::new())
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
                metadata: Some(ene_api_types::provider::ModelMetadata {
                    source: format!("{endpoint}/models"),
                    scope: ene_api_types::provider::MetadataScope::Route,
                    ..Default::default()
                }),
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
