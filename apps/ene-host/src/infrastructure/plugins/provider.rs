//! Wasm plugins use the invocation-scoped host HTTP broker.
use super::*;
use crate::infrastructure::llm::plugin::broker;
use crate::{
    domain::Connection,
    llm::{self, Invocation, LlmProvider, Model, Policy},
};
use base64::Engine;
use ene_plugin::{HttpChunk, HttpRequest};
use futures_util::{StreamExt, stream::BoxStream};

pub struct Provider {
    pub manager: Manager,
    pub id: String,
}
impl Manager {
    pub fn provider(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        let (package, _) = self.provider_entry(id)?;
        self.ready(&package, None).ok()?;
        Some(Arc::new(Provider {
            manager: self.clone(),
            id: id.into(),
        }))
    }
    fn cached_provider_bundle(&self, id: &str) -> Result<Bundle> {
        let (package, entrypoint) = self.provider_entry(id).ok_or_else(unsupported)?;
        let entries = self.entries.read().map_err(failure)?;
        let entry = entries.get(&package).ok_or_else(unsupported)?;
        super::bundle(&entry.package, &entrypoint)
    }
    pub(crate) fn provider_policy(&self, c: &Connection) -> Result<Policy> {
        self.cached_provider_bundle(&c.adapter)?.policy(c)
    }
    pub(crate) fn provider_identity(&self, c: &Connection) -> Result<Policy> {
        Ok(self.cached_provider_bundle(&c.adapter)?.identity_policy(c))
    }
    pub async fn enable_provider(&self, id: &str, enabled: bool) -> Result<()> {
        let (package, _) = self.provider_entry(id).ok_or_else(unsupported)?;
        let revision = self
            .entries
            .read()
            .map_err(failure)?
            .get(&package)
            .ok_or_else(unsupported)?
            .package
            .revision
            .clone();
        self.change_enabled(
            package,
            EnablePluginRequest {
                id: id.into(),
                enabled,
                revision,
                ..Default::default()
            },
        )
        .await
    }
}
struct ScopedAuthorization {
    manager: Manager,
    package_id: String,
    revision: String,
    cancel: CancellationToken,
}
impl llm::Authorization for ScopedAuthorization {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            if self.cancel.is_cancelled() {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            self.manager.ready(&self.package_id, Some(&self.revision))?;
            Ok(())
        })
    }
}
struct HttpStream {
    response: crate::llm::Response,
    _permit: tokio::sync::OwnedSemaphorePermit,
    status: u16,
    headers: BTreeMap<String, String>,
    pending: std::collections::VecDeque<u8>,
}
struct ProviderServices {
    manager: Manager,
    package_id: String,
    revision: String,
    input: Arc<Invocation>,
    broker: Arc<broker::Broker>,
    streams: Mutex<BTreeMap<String, HttpStream>>,
}
impl ProviderServices {
    async fn chunk(&self, handle: String) -> std::result::Result<Value, Failure> {
        self.check().await?;
        let mut streams = self.streams.lock().await;
        let stream = streams
            .get_mut(&handle)
            .ok_or_else(|| Failure::new("unknown_stream"))?;
        let done = if stream.pending.is_empty() {
            loop {
                self.check().await?;
                match stream
                    .response
                    .chunk()
                    .await
                    .map_err(|_| Failure::new("http_failed"))?
                {
                    Some(data) => {
                        stream.pending.extend(data);
                        if !stream.pending.is_empty() {
                            break false;
                        }
                    }
                    None => break true,
                }
            }
        } else {
            false
        };
        self.check().await?;
        let data: Vec<_> = stream
            .pending
            .drain(..stream.pending.len().min(64 * 1024))
            .collect();
        let chunk = HttpChunk {
            handle: handle.clone(),
            status: stream.status,
            headers: stream.headers.clone(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            done,
        };
        if done {
            streams.remove(&handle);
        }
        serde_json::to_value(chunk).map_err(|_| Failure::new("invalid_http_response"))
    }
    async fn open(&self, request: HttpRequest) -> std::result::Result<Value, Failure> {
        self.check().await?;
        let handle = uuid::Uuid::new_v4().to_string();
        {
            let mut streams = self.streams.lock().await;
            if streams.len() >= 8 {
                return Err(Failure::new("stream_limit"));
            }
            let (response, permit) = self.broker.request(request).await.map_err(|error| {
                Failure::new(match error.kind {
                    ErrorKind::PermissionRequired => "permission_required",
                    ErrorKind::InvalidRequest => "invalid_request",
                    ErrorKind::Unavailable => "unavailable",
                    _ => "http_failed",
                })
            })?;
            let status = response.status().as_u16();
            let mut headers = BTreeMap::new();
            if let Some(content_type) = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
            {
                headers.insert("content-type".into(), content_type.into());
            }
            streams.insert(
                handle.clone(),
                HttpStream {
                    response,
                    _permit: permit,
                    status,
                    headers,
                    pending: Default::default(),
                },
            );
        }
        self.chunk(handle).await
    }
}
impl Services for ProviderServices {
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async {
            self.manager
                .ready(&self.package_id, Some(&self.revision))
                .map_err(|_| Failure::new("revoked"))?;
            if *self.input.cancellation().borrow() {
                return Err(Failure::new("cancelled"));
            }
            self.input
                .authorization()
                .check()
                .await
                .map_err(|_| Failure::new("permission_required"))
        })
    }
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async move {
            self.check().await?;
            match request {
                ServiceRequest::HttpOpen { request } => self.open(request).await,
                ServiceRequest::HttpRead { handle } => self.chunk(handle).await,
                ServiceRequest::HttpClose { handle } => {
                    self.streams.lock().await.remove(&handle);
                    Ok(Value::Null)
                }
                ServiceRequest::Configuration => {
                    serde_json::from_str(&self.input.connection().options.json)
                        .map_err(|_| Failure::new("invalid_configuration"))
                }
                ServiceRequest::Log { message } if message.len() <= 4096 => Ok(Value::Null),
                _ => Err(Failure::new("unsupported_service")),
            }
        })
    }
}
impl Provider {
    async fn execute(
        &self,
        input: Invocation,
        models: bool,
    ) -> Result<BoxStream<'static, Result<Event>>> {
        let (package_id, entrypoint) = self
            .manager
            .provider_entry(&self.id)
            .ok_or_else(unsupported)?;
        let entry = self.manager.ready(&package_id, None)?;
        let bundle = self.manager.provider_bundle(&self.id)?;
        bundle.policy(input.connection())?;
        let mut watch = input.cancellation();
        let input = Arc::new(input.restrict(Arc::new(ScopedAuthorization {
            manager: self.manager.clone(),
            package_id: package_id.clone(),
            revision: entry.package.revision.clone(),
            cancel: entry.cancellation.clone(),
        })));
        let operation = if models {
            Operation::ListModels {
                endpoint: input.connection().endpoint.clone(),
                options: serde_json::from_str(&input.connection().options.json)
                    .map_err(|_| Error::invalid())?,
            }
        } else {
            let request = input.generation()?;
            Operation::Generate {
                request: ene_plugin::GenerationRequest {
                    endpoint: request.endpoint().into(),
                    options: request.options().clone(),
                    model: request.model().into(),
                    preamble: request.preamble().into(),
                    messages: request
                        .messages()
                        .iter()
                        .map(|m| ene_plugin::provider::ChatMessage {
                            role: m.role.as_str().into(),
                            text: m.text.clone(),
                        })
                        .collect(),
                    max_tokens: request.max_tokens(),
                    tools: Vec::new(),
                },
            }
        };
        let services = Arc::new(ProviderServices {
            manager: self.manager.clone(),
            package_id,
            revision: entry.package.revision.clone(),
            input: input.clone(),
            broker: Arc::new(broker::Broker {
                bundle,
                input: input.clone(),
                calls: Default::default(),
                active: Arc::new(tokio::sync::Semaphore::new(1)),
            }),
            streams: Mutex::new(BTreeMap::new()),
        });
        let package = entry.package.clone();
        let cancel = entry.cancellation.child_token();
        let (out, mut output) = mpsc::channel(16);
        self.manager.supervisor.spawn(async move {
            let run=async {
                let mut worker=Instance::load(package,entrypoint,Target::Host,entry.generation,&cancel).await.map_err(failure)?;
                let (tx,mut rx)=mpsc::channel(16);
                let execute=worker.invoke(operation,services.clone(),tx,&cancel);
                let forward=async {while let Some(event)=rx.recv().await {out.send(Ok(event)).await.map_err(|_|Error::unavailable())?;}Ok::<_,Error>(())};
                let (result,forward)=tokio::join!(execute,forward);
                worker.stop().await;
                result.map_err(failure)?;forward?;Ok::<_,Error>(())
            };
            let cancellation=async {if !*watch.borrow() {let _=watch.wait_for(|v|*v).await;}cancel.cancel();};
            let result=tokio::select! {biased;_ = cancellation=>Err(Error::new(ErrorKind::ExternalFailure)),_ = out.closed()=>Err(Error::unavailable()),result=run=>result};
            cancel.cancel();
            if let Err(error)=result {let _=out.send(Err(error)).await;}
        }).map_err(failure)?;
        Ok(Box::pin(
            async_stream::stream! {while let Some(event)=output.recv().await {yield event;}},
        ))
    }
}
impl LlmProvider for Provider {
    fn policy(&self, c: &Connection) -> Result<Policy> {
        self.manager.provider_policy(c)
    }
    fn generate(
        &self,
        input: Invocation,
    ) -> BoxFuture<'_, Result<BoxStream<'static, Result<llm::Event>>>> {
        Box::pin(async move {
            let mut stream = self.execute(input, false).await?;
            Ok(Box::pin(async_stream::try_stream! {
                let mut finished=false;
                while let Some(event)=stream.next().await {
                    match event? {
                        Event::Delta{text}=>yield llm::Event::Delta(text),
                        Event::Finished{reason}=>{finished=true;yield llm::Event::Finished(reason);},
                        _=>Err(unsupported())?,
                    }
                }
                // An aborted forwarding task can close the channel before runtime validation finishes.
                if !finished {Err(Error::new(ErrorKind::ExternalFailure))?;}
            }) as _)
        })
    }
    fn models(&self, input: Invocation) -> BoxFuture<'_, Result<Vec<Model>>> {
        Box::pin(async move {
            let (package, entrypoint) = self
                .manager
                .provider_entry(&self.id)
                .ok_or_else(unsupported)?;
            let entry = self.manager.ready(&package, None)?;
            if entry
                .capabilities
                .get(&entrypoint)
                .is_some_and(|c| c["model_list"] == false)
            {
                self.policy(input.connection())?;
                input.authorization().check().await?;
                return Ok(self.manager.provider_bundle(&self.id)?.manifest.models);
            }
            let mut stream = self.execute(input, true).await?;
            let mut result = None;
            while let Some(event) = stream.next().await {
                match event? {
                    Event::Models { models } if result.is_none() => result = Some(models),
                    _ => return Err(Error::new(ErrorKind::ExternalFailure)),
                }
            }
            let models = result.ok_or_else(|| Error::new(ErrorKind::ExternalFailure))?;
            llm::validate_models(&models)?;
            Ok(models)
        })
    }
}
