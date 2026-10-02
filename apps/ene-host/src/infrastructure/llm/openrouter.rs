//! Builtin adapter; OpenRouter routing options never enter the provider-neutral port.
use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
};
use futures_util::{StreamExt, stream::BoxStream};
use serde_json::{Value, json};
pub const MODEL: &str = "openai/gpt-6-luna";
pub const ENDPOINT: &str = "https://openrouter.ai/api/v1";
pub fn descriptor() -> Descriptor {
    Descriptor {
        id: "openrouter".into(),
        name: "OpenRouter".into(),
        kind: "builtin".into(),
        endpoint: ENDPOINT.into(),
        options: json!({"only":["openai"],"allow_fallbacks":false}),
        settings: vec![],
        requires_credentials: true,
        environment_variable: Some("OPENROUTER_API_KEY".into()),
        destinations: vec!["https://openrouter.ai".into(), "openai".into()],
        state: "ready".into(),
        error: None,
        revision: "builtin-v1".into(),
        models: vec![Model {
            id: MODEL.into(),
            name: MODEL.into(),
        }],
        capabilities: Capabilities {
            streaming_text: true,
            model_list: true,
        },
    }
}
pub fn policy(c: &Connection) -> Result<Policy> {
    let options: OpenRouterOptions =
        serde_json::from_value(c.options.clone()).map_err(|_| Error::invalid())?;
    if c.kind != "llm"
        || c.endpoint != ENDPOINT
        || options.only != ["openai"]
        || options.allow_fallbacks
    {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    Ok(Policy {
        fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":c.options}),
        destinations: descriptor().destinations,
        requires_credentials: true,
        environment_variable: Some("OPENROUTER_API_KEY".into()),
    })
}
pub async fn generate(input: Invocation) -> Result<BoxStream<'static, Result<Event>>> {
    input.authorization.check().await?;
    let secret = input
        .secret
        .ok_or_else(|| Error::new(ErrorKind::CredentialMissing))?;
    Ok(Box::pin(async_stream::try_stream! {
     use rig_core::{completion::CompletionRequest,streaming::{Item,StreamEvent}};
     let mut messages=input.request.messages.into_iter();
     let mut all=messages.by_ref().collect::<Vec<_>>();let prompt=all.pop().ok_or_else(Error::invalid)?.text;
     let history=all.into_iter().map(|m|if m.role=="user"{rig_core::message::Message::user(m.text)}else{rig_core::message::Message::assistant(m.text)});
     let request=CompletionRequest::new(prompt).preamble(input.request.preamble).messages(history).max_tokens(input.request.max_tokens).additional_params(json!({"provider":input.request.options}));
     let client=rig_core::providers::openrouter::new(secret.value);
     let mut stream=client.chat(input.request.model).stream(request).map_err(|_|Error::new(ErrorKind::ExternalFailure))?;
     while let Some(item)=stream.next().await{if let Item::Event(StreamEvent::Text{text,..})=item.map_err(|_|Error::new(ErrorKind::ExternalFailure))?{yield Event::Delta(text);}}
     let response=stream.finish().await.map_err(|_|Error::new(ErrorKind::ExternalFailure))?;
     yield Event::Finished(if matches!(response.finish_reason(),Some(rig_core::completion::FinishReason::Stop)){"stop"}else{"incomplete"}.into());
    }))
}
pub async fn models(input: Invocation) -> Result<Vec<Model>> {
    input.authorization.check().await?;
    let secret = input
        .secret
        .ok_or_else(|| Error::new(ErrorKind::CredentialMissing))?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::unavailable())?
        .get(format!("{ENDPOINT}/models"))
        .bearer_auth(secret.value)
        .send()
        .await
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?
        .error_for_status()
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    let value: Value = response
        .json()
        .await
        .map_err(|_| Error::new(ErrorKind::ExternalFailure))?;
    Ok(value["data"]
        .as_array()
        .ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?
        .iter()
        .filter_map(|v| {
            Some(Model {
                id: v["id"].as_str()?.into(),
                name: v["name"].as_str()?.into(),
            })
        })
        .collect())
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenRouterOptions {
    only: Vec<String>,
    allow_fallbacks: bool,
}
