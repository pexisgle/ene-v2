//! One isolated worker, ordered invocations, bounded IPC and cancellation-aware services.
use crate::transport::{Reader, Writer};
use crate::{Error, Result, limits::*, package::Installed};
use ene_platform::sandbox::Child;
use ene_plugin::*;
use futures_util::future::BoxFuture;
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub trait Services: Send + Sync {
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<serde_json::Value, Failure>>;
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>>;
}
pub struct DenyServices;
impl Services for DenyServices {
    fn handle(
        &self,
        _: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<serde_json::Value, Failure>> {
        Box::pin(async { Err(Failure::new("permission_required")) })
    }
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}
pub struct Worker {
    child: Child,
    input: Writer<HostMessage>,
    output: Reader<WorkerMessage>,
    installed: Installed,
    entrypoint: String,
    generation: u64,
    pub capabilities: serde_json::Value,
}
pub fn spawn(
    installed: &Installed,
    entrypoint: &str,
    runner: &Path,
    target: Target,
) -> Result<Child> {
    let entry = installed.package.entry(entrypoint)?;
    if entry.target != target {
        return Err(Error::Unsupported);
    }
    if crate::Package::load_snapshot(&installed.root)?.revision != installed.package.revision {
        return Err(Error::Changed);
    }
    ene_platform::sandbox::spawn(
        &installed.root,
        runner,
        &[
            "--package".into(),
            "/plugin".into(),
            "--entrypoint".into(),
            entrypoint.into(),
        ],
    )
    .map_err(|_| Error::Unavailable)
}
impl Worker {
    pub async fn start(
        installed: Installed,
        entrypoint: String,
        runner: &Path,
        target: Target,
        generation: u64,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let mut child = spawn(&installed, &entrypoint, runner, target)?;
        let pipes = (
            child.stdout.take().ok_or(Error::Protocol)?,
            child.stdin.take().ok_or(Error::Protocol)?,
        );
        let connected = tokio::select! { biased;
            _ = cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(INITIALIZE_TIMEOUT, crate::transport::host(pipes.0, pipes.1)) =>
                result.map_err(|_| Error::Timeout).and_then(|v| v.map_err(|_| Error::Protocol)),
        };
        let (input, output) = match connected {
            Ok(v) => v,
            Err(e) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(e);
            }
        };
        let mut worker = Self {
            input,
            output,
            child,
            installed,
            entrypoint: entrypoint.clone(),
            generation,
            capabilities: serde_json::Value::Null,
        };
        let call = Call {
            request_id: uuid::Uuid::new_v4().to_string(),
            generation,
        };
        let start = async {
            worker
                .input
                .send(&HostMessage::Initialize {
                    contract_version: CONTRACT_VERSION,
                    call: call.clone(),
                    entrypoint,
                })
                .await
                .map_err(|_| Error::Protocol)?;
            match worker.output.receive().await.map_err(|_| Error::Protocol)? {
                Some(WorkerMessage::Ready {
                    contract_version,
                    call: received,
                    capabilities,
                }) if contract_version == CONTRACT_VERSION && received == call => {
                    if serde_json::to_vec(&capabilities)?.len() > MAX_CHUNK {
                        return Err(Error::Protocol);
                    }
                    let entry = worker.installed.package.entry(&worker.entrypoint)?;
                    if entry.provides.contains(&Extension::Llm) {
                        let c: LlmCapabilities = serde_json::from_value(capabilities.clone())?;
                        if !c.streaming_text {
                            return Err(Error::Unsupported);
                        }
                    }
                    worker.capabilities = capabilities;
                    Ok(())
                }
                _ => Err(Error::Protocol),
            }
        };
        let result = tokio::select! { biased; _ = cancel.cancelled() => Err(Error::Cancelled), r = tokio::time::timeout(INITIALIZE_TIMEOUT, start) => r.unwrap_or(Err(Error::Timeout)) };
        if let Err(error) = result {
            worker.stop().await;
            return Err(error);
        }
        Ok(worker)
    }
    pub async fn invoke(
        &mut self,
        request_id: String,
        mut operation: Operation,
        services: Arc<dyn Services>,
        events: mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<serde_json::Value> {
        self.installed.package.verify()?;
        let entry = self.installed.package.entry(&self.entrypoint)?.clone();
        if !entry.provides.contains(&operation.extension()) {
            return Err(Error::Unsupported);
        }
        if let Operation::Speech {
            input: SpeechInput::Synthesize { generation, .. },
        } = &mut operation
        {
            *generation = self.generation;
        }
        let mut validator = crate::contract::Events::new(&operation);
        let extension = operation.extension();
        let call = Call {
            request_id,
            generation: self.generation,
        };
        let work = async {
            services.check().await.map_err(|_| Error::Denied)?;
            self.input
                .send(&HostMessage::Invoke {
                    call: call.clone(),
                    operation,
                })
                .await
                .map_err(|_| Error::Protocol)?;
            let mut sequence = 0;
            let mut service_ids = BTreeSet::new();
            loop {
                let message = self
                    .output
                    .receive()
                    .await
                    .map_err(|_| Error::Protocol)?
                    .ok_or(Error::Protocol)?;
                services.check().await.map_err(|_| Error::Denied)?;
                match message {
                    WorkerMessage::Event {
                        call: received,
                        sequence: next,
                        event,
                    } if received == call
                        && next == sequence
                        && event.extension() == extension
                        && sequence < MAX_STREAM_EVENTS =>
                    {
                        validator.accept(&event)?;
                        sequence += 1;
                        if serde_json::to_vec(&event)?.len() > MAX_CHUNK {
                            return Err(Error::Protocol);
                        }
                        events.send(event).await.map_err(|_| Error::Cancelled)?;
                    }
                    WorkerMessage::Service {
                        call: received,
                        service_id,
                        request,
                    } if received == call => {
                        if service_id.is_empty()
                            || service_id.len() > 100
                            || service_ids.len() >= MAX_SERVICE_CALLS
                            || !service_ids.insert(service_id.clone())
                        {
                            return Err(Error::Protocol);
                        }
                        crate::contract::service_request(&request)?;
                        let result = if entry.uses.contains(&request.service()) {
                            services.handle(request).await
                        } else {
                            Err(Failure::new("undeclared_service"))
                        };
                        self.input
                            .send(&HostMessage::ServiceResult {
                                call: call.clone(),
                                service_id,
                                result,
                            })
                            .await
                            .map_err(|_| Error::Protocol)?;
                    }
                    WorkerMessage::Complete {
                        call: received,
                        result,
                    } if received == call => {
                        validator.finish()?;
                        return Ok(result);
                    }
                    WorkerMessage::Failed { call: received, .. } if received == call => {
                        return Err(Error::Protocol);
                    }
                    _ => return Err(Error::Protocol),
                }
            }
        };
        let result = tokio::select! { biased; _ = cancel.cancelled() => Err(Error::Cancelled), _ = events.closed() => Err(Error::Cancelled), r = tokio::time::timeout(EXECUTION_TIMEOUT, work) => r.unwrap_or(Err(Error::Timeout)) };
        if result.is_err() {
            self.stop().await;
        }
        result
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub async fn stop(&mut self) {
        let graceful = async {
            let _ = self.input.send(&HostMessage::Shutdown).await;
            self.child.wait().await
        };
        if tokio::time::timeout(Duration::from_secs(3), graceful)
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}
#[derive(Clone, Default)]
pub struct Supervisor {
    tracker: tokio_util::task::TaskTracker,
    tasks: Arc<std::sync::Mutex<Vec<tokio::task::AbortHandle>>>,
}
impl Supervisor {
    pub fn spawn(
        &self,
        task: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let mut tasks = self.tasks.lock().map_err(|_| Error::Unavailable)?;
        if self.tracker.is_closed() {
            return Err(Error::Unavailable);
        }
        tasks.retain(|t| !t.is_finished());
        if tasks.len() >= MAX_WORKERS {
            return Err(Error::Unavailable);
        }
        tasks.push(self.tracker.spawn(task).abort_handle());
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.tracker.close();
        if tokio::time::timeout(Duration::from_secs(4), self.tracker.wait())
            .await
            .is_err()
        {
            if let Ok(tasks) = self.tasks.lock() {
                for task in tasks.iter() {
                    task.abort();
                }
            }
            self.tracker.wait().await;
        }
    }
}
