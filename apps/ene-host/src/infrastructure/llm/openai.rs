//! Builtin adapter for the official OpenAI Responses API.
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
        destinations: vec!["https://api.openai.com".into()],
        state: "ready".into(),
        error: None,
        revision: "builtin-openai-v1".into(),
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
        fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":c.options}),
        destinations: descriptor().destinations,
        requires_credentials: true,
        environment_variable: Some("OPENAI_API_KEY".into()),
    })
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::unavailable())
}

fn client(secret: String, endpoint: &str) -> Result<OpenAI> {
    Ok(OpenAIConfig::new(secret)
        .with_base_url(endpoint)
        .connect(rig_reqwest::ReqwestClient::from(http_client()?)))
}

pub async fn generate(input: Invocation) -> Result<BoxStream<'static, Result<Event>>> {
    input.authorization.check().await?;
    let secret = input
        .secret
        .ok_or_else(|| Error::new(ErrorKind::CredentialMissing))?;
    // The builtin never reads OPENAI_BASE_URL or plugin routing options.
    stream(input.request, client(secret.value, ENDPOINT)?)
}

fn stream(input: Generation, client: OpenAI) -> Result<BoxStream<'static, Result<Event>>> {
    use rig_core::{
        completion::CompletionRequest,
        streaming::{Item, StreamEvent},
    };
    let mut all = input.messages;
    let prompt = all.pop().ok_or_else(Error::invalid)?.text;
    let history = all.into_iter().map(|m| {
        if m.role == "user" {
            rig_core::message::Message::user(m.text)
        } else {
            rig_core::message::Message::assistant(m.text)
        }
    });
    let request = CompletionRequest::new(prompt)
        .preamble(input.preamble)
        .messages(history)
        .max_tokens(input.max_tokens)
        .additional_params(json!({"store":false}));
    let mut stream = client
        .responses(input.model)
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
    input.authorization.check().await?;
    let secret = input
        .secret
        .ok_or_else(|| Error::new(ErrorKind::CredentialMissing))?;
    fetch_models(secret.value, ENDPOINT).await
}

async fn fetch_models(secret: String, endpoint: &str) -> Result<Vec<Model>> {
    let response = http_client()?
        .get(format!("{endpoint}/models"))
        .bearer_auth(secret)
        .send()
        .await
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?
        .error_for_status()
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    let value: Value = response
        .json()
        .await
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
                metadata: None,
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

#[cfg(test)]
mod tests;
