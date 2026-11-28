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
    completion::{CompletionRequest, CompletionResponse, Message, ToolDefinition},
    error::ProviderError,
    message::{
        DocumentSourceKind, Image, ImageMediaType, StopReason, ToolCall, ToolResultContent,
        UserContent,
    },
    providers::openai::{OpenAI, OpenAIConfig},
};
use std::{future::Future, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Models {
    http: reqwest::Client,
    db: sqlx::SqlitePool,
    pub vectors: crate::vectors::Vectors,
}

#[derive(Clone)]
pub(crate) struct Model {
    pub selection: ModelSelection,
    pub connection: Connection,
    pub client: OpenAI,
    pub plugin: Option<PluginProvider>,
}

impl Models {
    pub(crate) fn new(db: sqlx::SqlitePool) -> Result<Self> {
        Ok(Self {
            db,
            vectors: crate::vectors::Vectors::default(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(Duration::from_secs(90))
                .build()?,
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
        let (client, plugin) = match connection.kind.as_known() {
            Some(ProviderKind::OpenaiCompatible) => {
                let secret = self.credential(&connection).await?.unwrap_or_default();
                let config = OpenAIConfig::new(secret).with_base_url(endpoint);
                (
                    config.connect(rig_reqwest::ReqwestClient::from(self.http.clone())),
                    None,
                )
            }
            Some(ProviderKind::Plugin) => {
                let plugin =
                    PluginProvider::open(self.db.clone(), self, connection.clone()).await?;
                let client = OpenAIConfig::new("plugin")
                    .with_base_url(endpoint)
                    .connect(plugin.clone());
                (client, Some(plugin))
            }
            _ => return Err(invalid("connection cannot serve this model role")),
        };
        Ok(Model {
            selection,
            connection,
            client,
            plugin,
        })
    }
}

pub(crate) fn receiver(connection: &Connection) -> Result<String> {
    store::fingerprint(&(
        &connection.id,
        connection.kind,
        &connection.endpoint,
        &connection.plugin_id,
        &connection.entrypoint,
        &connection.command,
        &connection.args,
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

impl Model {
    pub(crate) async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let response = self
            .client
            .chat(&self.selection.model)
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
        if let Some(plugin) = &self.plugin {
            return plugin.models().await;
        }
        Ok(self
            .client
            .list_models()
            .await
            .map_err(provider_error)?
            .data
            .into_iter()
            .map(|model| model.id)
            .collect())
    }

    pub(crate) async fn embed(
        &self,
        inputs: Vec<String>,
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<f64>>> {
        if let Some(plugin) = &self.plugin {
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
        let adapter = self.client.embedding(&self.selection.model, None);
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

    pub(crate) async fn respond<F, O, T>(
        &self,
        prompt: Prompt<'_>,
        tools: Vec<ToolDefinition>,
        cancel: &CancellationToken,
        progress: Option<tokio::sync::mpsc::Sender<Option<String>>>,
        mut invoke: F,
    ) -> Result<String>
    where
        F: FnMut(ToolCall) -> O,
        O: Future<Output = Result<T>>,
        T: Into<ToolOutput>,
    {
        let tool_bytes = serde_json::to_vec(&tools)?.len();
        let prompt = prompt.fit(self.budget().saturating_sub(tool_bytes + 2048))?;
        let mut request = CompletionRequest::new(format!(
            "[Current context]\n{}\n[Latest input]\n{}",
            prompt.dynamic, prompt.input
        ));
        request.chat_history.splice(
            0..0,
            std::iter::once(Message::system(prompt.system))
                .chain((!prompt.summary.is_empty()).then(|| {
                    Message::system(format!("Previous conversation summary: {}", prompt.summary))
                }))
                .chain(prompt.history.iter().map(|message| match message.role {
                    Role::User => Message::user(&message.text),
                    Role::Assistant => Message::assistant(&message.text),
                    Role::Trigger => Message::user(format!("[Host event] {}", message.text)),
                }))
                .map(Arc::new),
        );
        request.tools = tools.into_iter().map(Arc::new).collect();
        request.max_tokens = Some(1024);
        let mut tool_context = 0;
        let mut image_message: Option<usize> = None;
        for _ in 0..12 {
            let response = tokio::select! {
                () = cancel.cancelled() => anyhow::bail!("turn cancelled"),
                response = async {
                    let Some(progress) = &progress else { return self.complete(request.clone()).await; };
                    let adapter = self.client.chat(&self.selection.model);
                    let mut stream = adapter.stream(request.clone()).map_err(provider_error)?;
                    let mut text = String::new();
                    while let Some(item) = stream.next().await {
                        if let rig_core::streaming::Item::Event(rig_core::streaming::StreamEvent::Text { text: delta, .. }) = item.map_err(provider_error)? {
                            text.push_str(&delta);
                            if text.len() >= 12 {
                                let _closed = progress.send(Some(std::mem::take(&mut text))).await;
                            }
                        }
                    }
                    if !text.is_empty() { let _closed = progress.send(Some(text)).await; }
                    stream.finish().await.map_err(provider_error)
                } => response?,
            };
            match response.stop() {
                StopReason::Stop | StopReason::Length => return Ok(response.text()),
                StopReason::ToolUse => {
                    if let Some(progress) = &progress {
                        let _closed = progress.send(None).await;
                    }
                    let message = response.message().context("missing tool call message")?;
                    tool_context += serde_json::to_vec(&message)?.len();
                    request.chat_history.push(Arc::new(message));
                    for call in response.tool_calls() {
                        let output = if request
                            .tools
                            .iter()
                            .any(|tool| tool.name == call.function.name)
                        {
                            invoke(call.clone())
                                .await
                                .map(Into::into)
                                .unwrap_or_else(|_| {
                                    String::from("Operation failed or was refused.").into()
                                })
                        } else {
                            String::from("Tool unavailable for this turn.").into()
                        };
                        let text = truncate(&output.text, 12_000);
                        tool_context += text.len() + 128;
                        let mut content = vec![ToolResultContent::text(text)];
                        if let Some(frame) = output.frame {
                            use base64::Engine as _;
                            if let Some(index) = image_message.replace(request.chat_history.len())
                                && let Message::User { content } =
                                    Arc::make_mut(&mut request.chat_history[index])
                            {
                                for part in content {
                                    if let UserContent::ToolResult(result) = part {
                                        result.content.retain(|item| {
                                            !matches!(item, ToolResultContent::Image(_))
                                        });
                                    }
                                }
                            }
                            content.push(ToolResultContent::Image(Image {
                                data: DocumentSourceKind::Base64(
                                    base64::engine::general_purpose::STANDARD.encode(frame.jpeg),
                                ),
                                media_type: Some(ImageMediaType::JPEG),
                                ..Default::default()
                            }));
                        }
                        request
                            .chat_history
                            .push(Arc::new(Message::tool_results(vec![call.result(content)])));
                    }
                    ensure!(
                        tool_context + usize::from(image_message.is_some()) * 8192
                            <= self.budget() / 2,
                        "tool outputs exceed context"
                    );
                }
                _ => anyhow::bail!("unhandled completion status"),
            }
        }
        anyhow::bail!("turn exceeded tool-call limit")
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

fn provider_error(error: ProviderError) -> anyhow::Error {
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
