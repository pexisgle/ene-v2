use super::{broker, manifest::Bundle, sandbox};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Result, llm::*},
};
use ene_protocol::{
    ipc::{Reader, Writer},
    provider::{HostMessage, MAX_LINE, PluginMessage, VERSION},
};
use futures_util::stream::BoxStream;
use std::{sync::Arc, time::Duration};
use tokio::{
    process::{Child, ChildStdin, ChildStdout},
    sync::mpsc,
};
struct Process {
    _image: tempfile::TempDir,
    child: Child,
    input: Option<Writer<ChildStdin>>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
    poisoned: bool,
    output: Reader<ChildStdout>,
}
fn request_failure(request_id: &str, expected: &str) -> Error {
    Error::new(if request_id == expected {
        ErrorKind::ProviderFailure
    } else {
        ErrorKind::ExternalFailure
    })
}
fn broken() -> Error {
    Error::new(ErrorKind::ExternalFailure)
}
impl Process {
    async fn start(
        bundle: &Bundle,
        id: &str,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<(Self, Capabilities)> {
        let sandbox::Spawned { mut child, image } = sandbox::spawn(bundle)?;
        let mut p = Self {
            _image: image,
            input: Some(Writer::with_limit(
                child.stdin.take().ok_or_else(broken)?,
                MAX_LINE,
            )),
            cancel: cancel.clone(),
            poisoned: false,
            output: Reader::with_limit(child.stdout.take().ok_or_else(broken)?, MAX_LINE),
            child,
        };
        let work = async {
            p.write(&HostMessage::Initialize {
                request_id: id.into(),
                protocol_version: VERSION,
            })
            .await?;
            match p.read().await? {
                PluginMessage::Initialized {
                    request_id,
                    protocol_version,
                    capabilities,
                } if request_id == id
                    && protocol_version == VERSION
                    && capabilities.streaming_text =>
                {
                    Ok(Capabilities {
                        streaming_text: capabilities.streaming_text,
                        model_list: capabilities.model_list,
                    })
                }
                _ => Err(broken()),
            }
        };
        let mut cancel = cancel;
        let result = tokio::select! {biased;_=wait_cancel(&mut cancel)=>Err(Error::new(ErrorKind::PermissionRequired)),r=tokio::time::timeout(Duration::from_secs(5),work)=>r.unwrap_or_else(|_|Err(broken()))};
        match result {
            Ok(c) => Ok((p, c)),
            Err(e) => {
                if e.kind == ErrorKind::ExternalFailure {
                    p.poisoned = true;
                }
                p.stop(id).await;
                Err(e)
            }
        }
    }
    async fn read(&mut self) -> Result<PluginMessage> {
        self.output
            .receive()
            .await
            .map_err(|_| broken())?
            .ok_or_else(broken)
    }
    async fn write(&mut self, message: &HostMessage) -> Result<()> {
        // Oversized host input is a request error, not evidence of broken IPC.
        if serde_json::to_vec(message).map_err(|_| broken())?.len() >= MAX_LINE {
            let mut error = Error::invalid();
            error.reason = Some("input_limit");
            return Err(error);
        }
        self.poisoned = true;
        let sending = self.input.as_mut().ok_or_else(broken)?.send(message);
        let result = tokio::select! {biased;_=wait_cancel(&mut self.cancel)=>Err(Error::new(ErrorKind::PermissionRequired)),r=tokio::time::timeout(Duration::from_secs(3),sending)=>r.map_err(|_|broken())?.map_err(|_|broken())};
        if result.is_ok() {
            self.poisoned = false;
        }
        result
    }
    async fn stop(&mut self, id: &str) {
        // One shared deadline covers cooperative shutdown and forced process collection.
        let graceful = async {
            if !self.poisoned
                && let Some(input) = self.input.as_mut()
                && input
                    .send(&HostMessage::Cancel {
                        request_id: id.into(),
                    })
                    .await
                    .is_ok()
            {
                let _ = input.send(&HostMessage::Shutdown).await;
            }
            self.input.take();
            self.child.wait().await
        };
        if tokio::time::timeout(Duration::from_secs(3), graceful)
            .await
            .is_err()
        {
            self.input.take();
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}
enum Output {
    Event(Event),
    Models(Vec<Model>),
}
async fn run(
    bundle: Bundle,
    input: Arc<Invocation>,
    models: bool,
    out: mpsc::Sender<Result<Output>>,
) {
    let mut cancelled = input.cancellation();
    let start = Process::start(&bundle, input.id(), Some(cancelled.clone())).await;
    let (mut process, capabilities) = match start {
        Ok(p) => p,
        Err(e) => {
            let _ = out.send(Err(e)).await;
            return;
        }
    };
    let request = if models {
        HostMessage::ListModels {
            request_id: input.id().to_owned(),
            endpoint: input.connection().endpoint.clone(),
            options: input.connection().options.clone(),
        }
    } else {
        let r = match input.generation() {
            Ok(r) => r,
            Err(error) => {
                let _ = out.send(Err(error)).await;
                return;
            }
        };
        HostMessage::Generate {
            request_id: input.id().to_owned(),
            request: ene_protocol::provider::GenerationRequest {
                endpoint: r.endpoint().to_owned(),
                options: r.options().clone(),
                model: r.model().to_owned(),
                preamble: r.preamble().to_owned(),
                messages: r
                    .messages()
                    .iter()
                    .map(|m| ene_protocol::provider::ChatMessage {
                        role: m.role.as_str().into(),
                        text: m.text.clone(),
                    })
                    .collect(),
                max_tokens: r.max_tokens(),
            },
        }
    };
    let work = async {
        if *cancelled.borrow() {
            return Err(broken());
        }
        if models && !capabilities.model_list {
            out.send(Ok(Output::Models(bundle.manifest.models.clone())))
                .await
                .map_err(|_| broken())?;
            return Ok(());
        }
        process.write(&request).await?;
        let (tx, mut rx) = mpsc::channel::<Result<HostMessage>>(16);
        let mut http = false;
        let mut http_ids = std::collections::HashSet::new();
        // Abort the broker on every exit path, including malformed IPC and dropped consumer.
        struct Job(Option<tokio::task::JoinHandle<()>>);
        impl Drop for Job {
            fn drop(&mut self) {
                if let Some(j) = &self.0 {
                    j.abort();
                }
            }
        }
        let mut broker_job = Job(None);
        // Keep a single bounded writer alive while stdout is drained. A plugin can
        // emit more than a pipe's capacity from just one broker response chunk.
        let mut writer = process.input.take().ok_or_else(broken)?;
        process.poisoned = true; // Dropping an in-flight send requires closing stdin.
        let (written, mut acknowledgements) = mpsc::channel(1);
        let writing = async move {
            while let Some(response) = rx.recv().await {
                let response = response?;
                tokio::time::timeout(Duration::from_secs(3), writer.send(&response))
                    .await
                    .map_err(|_| broken())?
                    .map_err(|_| broken())?;
                if matches!(response, HostMessage::HttpResponse { done: true, .. }) {
                    written.send(()).await.map_err(|_| broken())?;
                }
            }
            Err::<(), _>(broken())
        };
        tokio::pin!(writing);
        loop {
            tokio::select! {
                biased;
                _ = out.closed() => return Err(Error::new(ErrorKind::PermissionRequired)),
                _ = cancelled.changed() => return Err(Error::new(ErrorKind::PermissionRequired)),
                result = &mut writing => return result,
                done = acknowledgements.recv(), if http => {
                    done.ok_or_else(broken)?;
                    http = false;
                    broker_job.0.take();
                }
                message = process.output.receive::<PluginMessage>() => {
                    let message = message.map_err(|_| broken())?.ok_or_else(broken)?;
                    match message {
                        PluginMessage::Delta { request_id, text } if !models && request_id == input.id() => {
                            if text.len() > 128_000 { return Err(broken()); }
                            out.send(Ok(Output::Event(Event::Delta(text)))).await.map_err(|_| broken())?;
                        }
                        PluginMessage::Finished { request_id, reason } if !models && request_id == input.id() => {
                            out.send(Ok(Output::Event(Event::Finished(reason)))).await.map_err(|_| broken())?;
                            return Ok(());
                        }
                        PluginMessage::Models { request_id, models: list } if models && request_id == input.id() => {
                            if list.len() > 4096 || list.iter().any(|m| m.id.is_empty() || m.id.len() > 512 || m.name.len() > 512) {
                                return Err(broken());
                            }
                            out.send(Ok(Output::Models(list))).await.map_err(|_| broken())?;
                            return Ok(());
                        }
                        PluginMessage::Error { request_id, .. } => {
                            // An upstream request failed; the plugin protocol remains usable.
                            return Err(request_failure(&request_id, input.id()));
                        }
                        request @ PluginMessage::HttpRequest { .. } => {
                            let PluginMessage::HttpRequest { ref request_id, ref http_id, .. } = request else { unreachable!() };
                            if request_id != input.id() || http || http_ids.len() >= 8 || !http_ids.insert(http_id.clone()) {
                                return Err(broken());
                            }
                            let bundle = bundle.clone();
                            let input = input.clone();
                            let channel = tx.clone();
                            broker_job.0 = Some(tokio::spawn(async move {
                                if let Err(error) = broker::send(bundle, input, request, channel.clone()).await {
                                    let _ = channel.send(Err(error)).await;
                                }
                            }));
                            http = true;
                        }
                        _ => return Err(broken()),
                    }
                }
            }
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(180), work)
        .await
        .unwrap_or_else(|_| Err(broken()));
    if let Err(e) = result {
        if e.kind == ErrorKind::ExternalFailure {
            process.poisoned = true;
        }
        let _ = tokio::time::timeout(Duration::from_millis(100), out.send(Err(e))).await;
    }
    process.stop(input.id()).await;
}
pub async fn generate(
    bundle: Bundle,
    input: Invocation,
    supervisor: Supervisor,
) -> Result<BoxStream<'static, Result<Event>>> {
    input.authorization().check().await?;
    let (tx, mut rx) = mpsc::channel(16);
    supervisor.spawn(run(bundle, Arc::new(input), false, tx))?;
    Ok(Box::pin(
        async_stream::stream! {while let Some(value)=rx.recv().await{yield value.and_then(|v|match v{Output::Event(e)=>Ok(e),_=>Err(broken())});}},
    ))
}
pub async fn models(
    bundle: Bundle,
    input: Invocation,
    supervisor: Supervisor,
) -> Result<Vec<Model>> {
    input.authorization().check().await?;
    let (tx, mut rx) = mpsc::channel(1);
    supervisor.spawn(run(bundle, Arc::new(input), true, tx))?;
    match rx.recv().await.ok_or_else(broken)?? {
        Output::Models(m) => Ok(m),
        _ => Err(broken()),
    }
}
#[cfg(test)]
mod collection_tests {
    use super::*;
    #[tokio::test]
    async fn unresponsive_process_is_killed_and_collected_after_three_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = serde_json::json!({"protocol_version":1,"id":"stubborn","name":"stubborn","version":"1","command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"endpoint":"https://example.com/api","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
        std::fs::write(dir.path().join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.path().join("provider.py"),"import sys,json,time\nm=json.loads(sys.stdin.readline())\nprint(json.dumps({'type':'initialized','request_id':m['request_id'],'protocol_version':1,'capabilities':{'streaming_text':True,'model_list':False}}),flush=True)\nwhile True:time.sleep(1)\n").unwrap();
        let bundle = super::super::manifest::load(dir.path()).unwrap();
        let result = Process::start(&bundle, "init", None).await;
        if super::super::sandbox::bubblewrap(std::env::var_os("PATH").as_deref()).is_err() {
            assert!(result.is_err());
            return;
        }
        let (mut p, _) = result.unwrap();
        let pid = p.child.id().unwrap();
        let start = std::time::Instant::now();
        p.stop("init").await;
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(p.child.try_wait().unwrap().is_some());
    }
}

/// Tracks invocation lifetimes independently of HTTP consumers and graceful server shutdown.
#[derive(Clone, Default)]
pub struct Supervisor {
    tracker: tokio_util::task::TaskTracker,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}
impl Supervisor {
    fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) -> Result<()> {
        let mut tasks = self.tasks.lock().map_err(|_| Error::unavailable())?;
        if self.tracker.is_closed() {
            return Err(Error::unavailable());
        }
        tasks.retain(|t| !t.is_finished());
        if tasks.len() >= 16 {
            return Err(Error::unavailable());
        }
        let task = self.tracker.spawn(task);
        tasks.push(task.abort_handle());
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.tracker.close();
        if tokio::time::timeout(Duration::from_secs(3), self.tracker.wait())
            .await
            .is_err()
        {
            if let Ok(tasks) = self.tasks.lock() {
                for t in tasks.iter() {
                    t.abort();
                }
            }
            self.tracker.wait().await;
        }
    }
}

async fn wait_cancel(cancel: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(cancel) = cancel {
        if !*cancel.borrow() {
            let _ = cancel.changed().await;
        }
    } else {
        std::future::pending::<()>().await;
    }
}
pub async fn probe_with_cancel(
    bundle: &Bundle,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Capabilities> {
    let id = uuid::Uuid::new_v4().to_string();
    let (mut process, capabilities) = Process::start(bundle, &id, Some(cancel)).await?;
    process.stop(&id).await;
    Ok(capabilities)
}

#[cfg(test)]
mod request_error_tests {
    use super::*;
    #[test]
    fn provider_error_is_request_scoped_but_mismatched_id_breaks_protocol() {
        assert_eq!(
            request_failure("request", "request").kind,
            ErrorKind::ProviderFailure
        );
        assert_eq!(
            request_failure("another", "request").kind,
            ErrorKind::ExternalFailure
        );
    }
}
