use super::sandbox::Child;
use super::{broker, manifest::Bundle, sandbox};
use ene_core::{
    domain::{Error, ErrorKind},
    ports::{Result, llm::*},
};
use ene_ipc::{
    provider::{Client, Codec, HttpBrokerServerShared, Provider, ProviderError},
    remoc::{self, ConnectExt, rtc::ServerShared},
};
use futures_util::stream::BoxStream;
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

struct Process {
    _image: tempfile::TempDir,
    child: Child,
    client: Option<Client>,
}
fn broken() -> Error {
    Error::new(ErrorKind::ExternalFailure)
}
fn call_error(error: ProviderError) -> Error {
    Error::new(match error {
        ProviderError::Failed => ErrorKind::ProviderFailure,
        ProviderError::PermissionDenied => ErrorKind::PermissionRequired,
        ProviderError::InvalidInput => ErrorKind::InvalidRequest,
        ProviderError::Unavailable => ErrorKind::Unavailable,
        ProviderError::Call(_) => ErrorKind::ExternalFailure,
    })
}
impl Process {
    async fn start(
        bundle: &Bundle,
        mut cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<(Self, Capabilities)> {
        let sandbox::Spawned { mut child, image } = sandbox::spawn(bundle)?;
        let input = child.stdout.take().ok_or_else(broken)?;
        let output = child.stdin.take().ok_or_else(broken)?;
        let mut process = Self {
            _image: image,
            child,
            client: None,
        };
        let work = async {
            let client: Client = remoc::Connect::io::<_, _, Client, Client, remoc::codec::Default>(
                remoc::Cfg::default(),
                input,
                output,
            )
            .consume()
            .await
            .map_err(|_| broken())?;
            process.client = Some(client);
            let capabilities = process
                .client
                .as_ref()
                .ok_or_else(broken)?
                .capabilities()
                .await
                .map_err(call_error)?;
            if !capabilities.streaming_text {
                return Err(broken());
            }
            Ok(capabilities)
        };
        let result = tokio::select! {biased;
            _ = wait_cancel(&mut cancel) => Err(Error::new(ErrorKind::PermissionRequired)),
            r = tokio::time::timeout(Duration::from_secs(5), work) => r.unwrap_or_else(|_| Err(broken())),
        };
        match result {
            Ok(capabilities) => Ok((process, capabilities)),
            Err(error) => {
                process.stop().await;
                Err(error)
            }
        }
    }
    async fn stop(&mut self) {
        self.client.take();
        if tokio::time::timeout(Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
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
    let mut cancel = Some(input.cancellation());
    let (mut process, capabilities) = match Process::start(&bundle, cancel.clone()).await {
        Ok(process) => process,
        Err(error) => {
            let _ = out.send(Err(error)).await;
            return;
        }
    };
    let work = async {
        input.authorization().check().await?;
        if models && !capabilities.model_list {
            out.send(Ok(Output::Models(bundle.manifest.models.clone())))
                .await
                .map_err(|_| broken())?;
            return Ok(());
        }
        let (server, callback) =
            HttpBrokerServerShared::<_, Codec>::new(Arc::new(broker::Broker {
                bundle: bundle.clone(),
                input: input.clone(),
                calls: Default::default(),
                active: tokio::sync::Semaphore::new(1),
            }));
        // Serving and cancellation belong to Remoc. This guard bounds the broker
        // server's lifetime to the host invocation, even if a plugin retains it.
        let _broker = AbortOnDropHandle::new(tokio::spawn(server.serve()));
        let client = process.client.as_ref().ok_or_else(broken)?;
        if models {
            let list = client
                .list_models(
                    input.connection().endpoint.clone(),
                    input.connection().options.clone(),
                    callback,
                )
                .await
                .map_err(call_error)?;
            if list.len() > 4096
                || list
                    .iter()
                    .any(|m| m.id.is_empty() || m.id.len() > 512 || m.name.len() > 512)
            {
                return Err(broken());
            }
            out.send(Ok(Output::Models(list)))
                .await
                .map_err(|_| broken())?;
        } else {
            let request = input.generation()?;
            let request = ene_ipc::provider::GenerationRequest {
                endpoint: request.endpoint().into(),
                options: request.options().clone(),
                model: request.model().into(),
                preamble: request.preamble().into(),
                max_tokens: request.max_tokens(),
                messages: request
                    .messages()
                    .iter()
                    .map(|message| ene_ipc::provider::ChatMessage {
                        role: message.role.as_str().into(),
                        text: message.text.clone(),
                    })
                    .collect(),
            };
            let (sender, mut receiver) = remoc::rch::mpsc::channel::<String, Codec>();
            let receive = async {
                while let Some(text) = receiver.recv().await.map_err(|_| broken())? {
                    if text.len() > 128_000 {
                        return Err(broken());
                    }
                    out.send(Ok(Output::Event(Event::Delta(text))))
                        .await
                        .map_err(|_| broken())?;
                }
                Ok(())
            };
            let (reason, ()) = tokio::try_join!(
                async {
                    client
                        .generate(request, callback, sender)
                        .await
                        .map_err(call_error)
                },
                receive
            )?;
            out.send(Ok(Output::Event(Event::Finished(reason))))
                .await
                .map_err(|_| broken())?;
        }
        Ok(())
    };
    let result = tokio::select! {biased;
        _ = out.closed() => Err(Error::new(ErrorKind::PermissionRequired)),
        _ = wait_cancel(&mut cancel) => Err(Error::new(ErrorKind::PermissionRequired)),
        r = tokio::time::timeout(Duration::from_secs(180), work) => r.unwrap_or_else(|_| Err(broken())),
    };
    if let Err(error) = result {
        let _ = tokio::time::timeout(Duration::from_millis(100), out.send(Err(error))).await;
    }
    process.stop().await;
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
    let (mut process, capabilities) = Process::start(bundle, Some(cancel)).await?;
    process.stop().await;
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unresponsive_process_is_killed_and_collected_after_three_seconds() {
        let child = tokio::process::Command::new("python3")
            .args(["-I", "-u", "-c", "import time\nwhile True: time.sleep(1)"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut process = Process {
            _image: tempfile::tempdir().unwrap(),
            child,
            client: None,
        };
        let pid = process.child.id().unwrap();
        let start = std::time::Instant::now();
        process.stop().await;
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(process.child.try_wait().unwrap().is_some());
    }
}
