//! Runtime providers share the invocation-scoped Remoc HTTP broker.
use super::*;
use crate::infrastructure::llm::plugin::broker;
use base64::Engine;
use ene_core::{
    domain::Connection,
    ports::llm::{self, Invocation, LlmProvider, Model, Policy},
};
use ene_ipc::{
    plugin::{HttpChunk, HttpRequest},
    provider::{Codec, HttpBroker, HttpResponsePart, ProviderError},
};
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
        self.change_enabled(package, PluginEnabled { enabled, revision })
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
    output: ene_ipc::remoc::rch::mpsc::Receiver<HttpResponsePart, Codec>,
    task: tokio::task::JoinHandle<std::result::Result<(), ProviderError>>,
    status: u16,
    headers: BTreeMap<String, String>,
    pending: std::collections::VecDeque<u8>,
}
impl Drop for HttpStream {
    fn drop(&mut self) {
        self.task.abort();
    }
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
                match stream
                    .output
                    .recv()
                    .await
                    .map_err(|_| Failure::new("http_failed"))?
                {
                    Some(HttpResponsePart::Head { status, headers }) if stream.status == 0 => {
                        stream.status = status;
                        stream.headers = headers;
                    }
                    Some(HttpResponsePart::Data(data)) if stream.status != 0 => {
                        stream.pending.extend(data);
                        if !stream.pending.is_empty() {
                            break false;
                        }
                    }
                    None if stream.status != 0 => {
                        (&mut stream.task)
                            .await
                            .map_err(|_| Failure::new("http_failed"))?
                            .map_err(|_| Failure::new("http_failed"))?;
                        break true;
                    }
                    _ => return Err(Failure::new("invalid_http_response")),
                }
            }
        } else {
            false
        };
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
        let request = ene_ipc::provider::HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request
                .body_base64
                .map(|body| base64::engine::general_purpose::STANDARD.decode(body))
                .transpose()
                .map_err(|_| Failure::new("invalid_body"))?,
        };
        let handle = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = ene_ipc::remoc::rch::mpsc::with_local_buffer::<HttpResponsePart, Codec>(4);
        {
            let mut streams = self.streams.lock().await;
            if streams.len() >= 8 {
                return Err(Failure::new("stream_limit"));
            }
            let broker = self.broker.clone();
            let task = tokio::spawn(async move { broker.request(request, tx).await });
            streams.insert(
                handle.clone(),
                HttpStream {
                    output: rx,
                    task,
                    status: 0,
                    headers: BTreeMap::new(),
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
                ServiceRequest::Configuration => Ok(self.input.connection().options.clone()),
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
                options: input.connection().options.clone(),
            }
        } else {
            let request = input.generation()?;
            Operation::Generate {
                request: ene_ipc::plugin::GenerationRequest {
                    endpoint: request.endpoint().into(),
                    options: request.options().clone(),
                    model: request.model().into(),
                    preamble: request.preamble().into(),
                    messages: request
                        .messages()
                        .iter()
                        .map(|m| ene_ipc::provider::ChatMessage {
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
                active: tokio::sync::Semaphore::new(1),
            }),
            streams: Mutex::new(BTreeMap::new()),
        });
        let installed = entry
            .package
            .install(&self.manager.cache)
            .map_err(failure)?;
        let runner = self.manager.runner.clone();
        let cancel = entry.cancellation.child_token();
        let (out, mut output) = mpsc::channel(16);
        self.manager.supervisor.spawn(async move {
            let run=async {
                let mut worker=Worker::start(installed,entrypoint,&runner,Target::Host,entry.generation,&cancel).await.map_err(failure)?;
                let (tx,mut rx)=mpsc::channel(16);
                let execute=worker.invoke(input.id().into(),operation,services.clone(),tx,&cancel);
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
                    if finished {Err(Error::new(ErrorKind::ExternalFailure))?;}
                    match event? {
                        Event::Delta{text}=>yield llm::Event::Delta(text),
                        Event::Finished{reason}=>{finished=true;yield llm::Event::Finished(reason);},
                        _=>Err(unsupported())?,
                    }
                }
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
