mod agent_platform;
mod response;

pub(crate) use agent_platform::publisher as google_publisher;

use crate::{
    error::{ApiError, invalid},
    plugin_provider::PluginProvider,
    store,
};
use anyhow::{Context, Result, ensure};
use connectrpc::ErrorCode;
use ene_core::mind::{Prompt, Role};
use ene_rpc::{Connection, ModelSelection, ProviderKind};
use futures_util::StreamExt as _;
use rig_core::{
    catalog::Catalog,
    completion::{CompletionRequest, CompletionResponse, Message, ToolDefinition},
    error::ProviderError,
    message::{
        DocumentSourceKind, Image, ImageMediaType, StopReason, ToolCall, ToolResultContent,
        UserContent,
    },
    providers::{
        openai::{OpenAI, OpenAIConfig},
        registry::{ProviderConfig, ProviderId},
    },
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Models {
    pub(crate) http: reqwest::Client,
    voicevox: crate::voicevox::Engine,
    db: sqlx::SqlitePool,
    pub vectors: crate::vectors::Vectors,
}

#[derive(Clone)]
pub(crate) struct Model {
    pub selection: ModelSelection,
    pub connection: Connection,
    adapter: Adapter,
    http: reqwest::Client,
}

#[derive(Clone)]
enum Adapter {
    Compatible(Box<OpenAI>),
    Native(Box<ProviderConfig>),
    Plugin {
        client: Box<OpenAI>,
        provider: Box<PluginProvider>,
    },
    Voicevox(crate::voicevox::Engine),
    Google(agent_platform::AgentPlatform),
}

impl Models {
    pub(crate) fn new(db: sqlx::SqlitePool) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self {
            db,
            vectors: crate::vectors::Vectors::default(),
            voicevox: crate::voicevox::Engine::new(http.clone()),
            http,
        })
    }

    pub(crate) async fn credential(&self, connection: &Connection) -> Result<Option<String>> {
        if let Some(name) = &connection.credential_env {
            return std::env::var(name).map(Some).map_err(|_| {
                ApiError(
                    ErrorCode::Unauthenticated,
                    "configured environment credential is missing",
                )
                .into()
            });
        }
        if !connection.credential_configured {
            return Ok(None);
        }
        let row = sqlx::query_as::<_, (String, String)>(
            "SELECT destination,slot FROM credentials WHERE connection_id=?",
        )
        .bind(&connection.id)
        .fetch_optional(&self.db)
        .await?;
        let account = row
            .filter(|(destination, _)| {
                receiver(connection).is_ok_and(|selected| selected == *destination)
            })
            .map(|(_, slot)| slot)
            .ok_or(ApiError(
                ErrorCode::Unauthenticated,
                "credential does not match its provider",
            ))?;
        let secret =
            tokio::task::spawn_blocking(move || crate::keyring::entry(&account)?.get_password())
                .await??;
        Ok(Some(secret))
    }

    pub(crate) async fn write_credential(
        &self,
        account: String,
        secret: Option<String>,
    ) -> Result<()> {
        tokio::task::spawn_blocking(move || {
            let entry = crate::keyring::entry(&account)?;
            if let Some(secret) = secret {
                entry.set_password(&secret)?;
            } else if let Err(error) = entry.delete_credential()
                && !matches!(error, keyring_core::Error::NoEntry)
            {
                return Err(error);
            }
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub(crate) async fn model(
        &self,
        selection: ModelSelection,
        connection: Connection,
    ) -> Result<Model> {
        if !connection.enabled {
            return Err(ApiError(ErrorCode::FailedPrecondition, "provider disabled").into());
        }
        check_endpoint(&connection.endpoint)?;
        let endpoint = connection.endpoint.trim_end_matches('/');
        let adapter = match connection.kind.as_known() {
            Some(ProviderKind::OpenaiCompatible) => {
                let secret = self.credential(&connection).await?.unwrap_or_default();
                let config = OpenAIConfig::new(secret).with_base_url(endpoint);
                Adapter::Compatible(Box::new(
                    config.connect(rig_reqwest::ReqwestClient::from(self.http.clone())),
                ))
            }
            Some(ProviderKind::Plugin) => {
                let plugin =
                    PluginProvider::open(self.db.clone(), self, connection.clone()).await?;
                let client = OpenAIConfig::new("plugin")
                    .with_base_url(endpoint)
                    .connect(plugin.clone());
                Adapter::Plugin {
                    client: Box::new(client),
                    provider: Box::new(plugin),
                }
            }
            Some(ProviderKind::Native)
                if connection.adapter == ene_domain::agent_platform::ADAPTER =>
            {
                let secret = self.credential(&connection).await?;
                Adapter::Google(agent_platform::AgentPlatform::new(
                    self.http.clone(),
                    &connection,
                    secret.as_deref(),
                )?)
            }
            Some(ProviderKind::Native) => {
                let provider = native_provider(&connection.adapter)?;
                let secret = self.credential(&connection).await?.unwrap_or_default();
                let config = provider
                    .config(secret)
                    .ok_or_else(|| invalid("unsupported native provider"))?
                    .with_base_url(endpoint);
                Adapter::Native(Box::new(config))
            }
            Some(ProviderKind::Voicevox) => Adapter::Voicevox(self.voicevox.clone()),
            _ => return Err(invalid("connection cannot serve this model role")),
        };
        Ok(Model {
            selection,
            connection,
            adapter,
            http: self.http.clone(),
        })
    }
}

pub(crate) fn receiver(connection: &Connection) -> Result<String> {
    let effective_receiver = if connection.kind == ProviderKind::Native
        && connection.adapter == ene_domain::agent_platform::ADAPTER
    {
        Some(google_publisher(connection)?)
    } else {
        None
    };
    store::fingerprint(&(
        &connection.id,
        connection.kind,
        &connection.adapter,
        &connection.endpoint,
        &connection.plugin_id,
        &connection.entrypoint,
        &connection.command,
        &connection.args,
        effective_receiver,
    ))
}

pub(crate) fn check_endpoint(endpoint: &str) -> Result<url::Url> {
    let url = url::Url::parse(endpoint)
        .ok()
        .filter(ene_core::policy::safe_url)
        .ok_or_else(|| invalid("invalid provider endpoint"))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain == "localhost",
        None => false,
    };
    if !(url.scheme() == "https" || loopback) || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("provider endpoint requires HTTPS or local HTTP"));
    }
    Ok(url)
}

pub(crate) fn native_provider(adapter: &str) -> Result<ProviderId> {
    ProviderId::catalog(adapter)
        .filter(|provider| provider.config("").is_some())
        .ok_or_else(|| invalid("unknown native provider"))
}

impl Model {
    pub(crate) fn openai(&self) -> Result<OpenAI> {
        match &self.adapter {
            Adapter::Compatible(client) | Adapter::Plugin { client, .. } => {
                Ok(client.as_ref().clone())
            }
            Adapter::Native(config) => match config.as_ref() {
                ProviderConfig::OpenAi(config) => Ok(config
                    .clone()
                    .connect(rig_reqwest::ReqwestClient::from(self.http.clone()))),
                _ => Err(invalid("role requires an OpenAI-compatible provider")),
            },
            _ => Err(invalid("role requires an OpenAI-compatible provider")),
        }
    }

    pub(crate) fn gemini(&self) -> Option<rig_core::providers::gemini::Gemini> {
        let Adapter::Native(config) = &self.adapter else {
            return None;
        };
        if let ProviderConfig::Gemini(config) = config.as_ref() {
            return Some(
                config
                    .clone()
                    .connect(rig_reqwest::ReqwestClient::from(self.http.clone())),
            );
        }
        None
    }

    pub(crate) fn plugin(&self) -> Option<&PluginProvider> {
        match &self.adapter {
            Adapter::Plugin { provider, .. } => Some(provider),
            _ => None,
        }
    }

    pub(crate) fn voicevox(&self) -> Option<&crate::voicevox::Engine> {
        match &self.adapter {
            Adapter::Voicevox(engine) => Some(engine),
            _ => None,
        }
    }

    fn completion(&self) -> Result<rig_core::DynModel<rig_core::operation::Completion>> {
        let model = &self.selection.model;
        match &self.adapter {
            Adapter::Google(google) => google.completion(model),
            Adapter::Native(config) => {
                let provider = native_provider(&self.connection.adapter)?;
                let http = rig_reqwest::ReqwestClient::from(self.http.clone());
                let facts = Catalog::builtin().facts(provider, model);
                Ok(config.completion_model(
                    model,
                    rig_core::http_client::DynHttpClient::new(http),
                    facts,
                ))
            }
            _ => Ok(self.openai()?.chat(model).erase()),
        }
    }

    pub(crate) async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let response = self
            .completion()?
            .call(request)
            .await
            .map_err(provider_error)?;
        ensure!(
            !response.stop().is_failure(),
            "provider returned a failed completion"
        );
        Ok(response)
    }

    pub(crate) async fn text(&self, system: &str, input: &str) -> Result<String> {
        let mut request = CompletionRequest::new(input);
        request
            .chat_history
            .insert(0, Arc::new(Message::system(system)));
        request.max_tokens = Some(4096);
        let output = self.complete(request).await?.text();
        ensure!(!output.trim().is_empty(), "provider returned no text");
        Ok(output)
    }

    pub(crate) async fn models(&self) -> Result<Vec<String>> {
        if let Some(plugin) = self.plugin() {
            return plugin.models().await;
        }
        if let Some(engine) = self.voicevox() {
            return engine.speakers(&self.connection).await;
        }
        if let Adapter::Google(google) = &self.adapter {
            return google.models().await;
        }
        let list = if let Adapter::Native(config) = &self.adapter {
            let http = rig_reqwest::ReqwestClient::from(self.http.clone());
            match config.as_ref().clone() {
                ProviderConfig::OpenAi(config) => config.connect(http).list_models().await,
                ProviderConfig::Anthropic(config) => config.connect(http).list_models().await,
                ProviderConfig::Gemini(config) => config.connect(http).list_models().await,
            }
        } else {
            self.openai()?.list_models().await
        }
        .map_err(provider_error)?;
        Ok(list.data.into_iter().map(|model| model.id).collect())
    }

    pub(crate) async fn embed(
        &self,
        inputs: Vec<String>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f64>>> {
        if let Some(plugin) = self.plugin() {
            let (value, _) = plugin
                .invoke(
                    ene_plugin::Operation::Embed {
                        request: ene_plugin::EmbeddingRequest {
                            endpoint: self.connection.endpoint.clone(),
                            options: plugin.options.clone(),
                            model: self.selection.model.clone(),
                            inputs,
                            dimensions: 0,
                        },
                    },
                    cancel,
                )
                .await?;
            let result: ene_plugin::EmbeddingResult = serde_json::from_value(value)?;
            return Ok(result
                .vectors
                .into_iter()
                .map(|vector| vector.into_iter().map(f64::from).collect())
                .collect());
        }
        let adapter = if let Some(gemini) = self.gemini() {
            gemini.embedding(&self.selection.model, None).erase()
        } else {
            self.openai()?
                .embedding(&self.selection.model, None)
                .erase()
        };
        let vectors = tokio::select! {
            () = cancel.cancelled() => anyhow::bail!("embedding cancelled"),
            reply = adapter.call(inputs) => reply.map_err(provider_error)?,
        };
        Ok(vectors
            .embeddings
            .into_iter()
            .map(|vector| vector.vec)
            .collect())
    }

    pub(crate) fn budget(&self) -> usize {
        match self.selection.context_bytes {
            0 => 48_000,
            bytes => (bytes as usize).clamp(4096, 1_048_576),
        }
    }
}

pub(crate) struct ToolOutput {
    pub text: String,
    pub frame: Option<ene_rpc::ScreenFrame>,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self { text, frame: None }
    }
}

pub(crate) fn truncate(text: &str, limit: usize) -> &str {
    &text[..text.floor_char_boundary(text.len().min(limit))]
}

pub(crate) fn provider_error(error: ProviderError) -> anyhow::Error {
    let status = match error {
        ProviderError::InvalidAuthentication(_) => Some(http::StatusCode::UNAUTHORIZED),
        ProviderError::ProviderResponse(response) => response.status,
        _ => None,
    };
    match status {
        Some(http::StatusCode::UNAUTHORIZED) => {
            ApiError(ErrorCode::Unauthenticated, "provider rejected credentials").into()
        }
        Some(http::StatusCode::FORBIDDEN) => {
            ApiError(ErrorCode::PermissionDenied, "provider denied access").into()
        }
        Some(http::StatusCode::TOO_MANY_REQUESTS) => {
            ApiError(ErrorCode::ResourceExhausted, "provider rate limit reached").into()
        }
        _ => ApiError(ErrorCode::Unavailable, "configured provider unavailable").into(),
    }
}
