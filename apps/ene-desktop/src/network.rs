mod connection;

use connection::{Clients, LocalHost};

mod observation;
mod operations;

use ene_domain::SubscriptionId;
use observation::{audio_worker, observe_enabled};
use operations::device_result;

mod actions;

use crate::renderer::Renderer;
use actions::call;
use anyhow::{Context, Result, ensure};
use connectrpc::client::{ClientConfig, HttpClient};
use ene_core::avatar::Command as Body;
use ene_rpc::{self as w, event::Payload};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub(crate) struct Network {
    pub discovery: PathBuf,
    pub local_host: Option<PathBuf>,
    pub assets: PathBuf,
    pub device: w::Device,
    pub renderer: Option<Renderer>,
    pub plugins: std::sync::Arc<[crate::plugins::ClientPlugin]>,
    pub plugin_paths: Vec<PathBuf>,
    pub stop: CancellationToken,
    pub speaker: bool,
    pub avatar_scale: f32,
    pub workspace: Option<std::sync::Arc<crate::workspace::Workspace>>,
}

#[derive(Clone)]
pub(crate) enum Action {
    History(String, u64),
    Send(w::SendRequest),
    Stop(w::StopRequest),
    StopWork(w::StopRequest),
    Answer(w::ApprovalAnswer),
    Settings,
    ClientPlugin { path: PathBuf, enabled: bool },
    Workspace(PathBuf),
    SetupProvider(w::Connection, Option<String>),
    SetupModel(w::Companion, w::ModelSelection),
    PutConnection(w::ConnectionChange),
    DeleteConnection(w::DeleteRequest),
    Credential(w::CredentialChange),
    Models(String),
    McpTools(String),
    Probe(w::ProbeRequest),
    Policy(w::PolicyChange),
    DeleteRule(w::DeleteRequest),
    PutPlugin(w::PluginChange),
    DeletePlugin(w::DeleteRequest),
    PutCharacter(w::CharacterChange),
    PutCompanion(w::CompanionChange),
    DeleteCompanion(w::DeleteRequest),
    ResetHistory,
    AvatarScale(f32),
    TestSpeech(String, String),
    Preview(w::PreviewRequest),
    Import(PathBuf),
    Export(w::ExportRequest, PathBuf),
    Activate(String),
    Eyes(bool),
    Input(bool),
    Speaker(bool),
    Interrupt(u64),
    Capture(u64, Vec<u8>, u32, bool),
}

pub(crate) enum Update {
    Connection(bool),
    Event(w::Event),
    History(String, u64, w::History),
    Settings(w::Settings),
    ClientPlugins(Vec<PathBuf>),
    Workspace(PathBuf),
    AvatarScale(f32),
    Transcribed(String),
    Models(Vec<String>),
    McpTools(Vec<String>),
    Audit(Vec<w::AuditEntry>),
    Audio(w::AudioOutput),
    Notice(String),
    SendResult { key: String, error: Option<String> },
    SetupFinished,
    SetupFailed(String),
}

impl Network {
    pub(crate) async fn run(
        mut self,
        mut requests: mpsc::Receiver<Action>,
        updates: mpsc::Sender<Update>,
    ) {
        let mut retry = Duration::from_millis(500);
        let mut installed = HashMap::new();
        let mut local = LocalHost::default();
        let mut speaker = self.speaker;
        loop {
            if self.stop.is_cancelled() {
                break;
            }
            if let Ok((clients, local)) = Clients::connect(&self.discovery) {
                let args = Session {
                    clients,
                    updates: &updates,
                    requests: &mut requests,
                    installed: &mut installed,
                    local,
                    speaker: &mut speaker,
                };
                if self.connected(args).await.is_ok() {
                    break;
                }
                retry = Duration::from_millis(500);
            }
            if let Some(executable) = self.local_host.as_deref() {
                local.ensure_running(executable, &self.discovery);
            }
            if updates.send(Update::Connection(false)).await.is_err() {
                break;
            }
            tokio::select! {
                () = self.stop.cancelled() => break,
                () = tokio::time::sleep(retry) => {}
            }
            retry = (retry * 2).min(Duration::from_secs(8));
        }
    }

    async fn connected(&mut self, args: Session<'_>) -> Result<()> {
        let Session {
            clients,
            updates,
            requests,
            installed,
            local,
            speaker,
        } = args;
        let registered = clients
            .device
            .register_device(w::DeviceChange {
                mutation: mutation(0),
                device: Some(self.device.clone()).into(),
                ..Default::default()
            })
            .await?
            .into_owned();
        let mut stream = clients
            .state
            .subscribe(w::SubscribeRequest::default())
            .await?;
        let Some(first) = stream.message::<w::Event>().await? else {
            anyhow::bail!("host subscription closed");
        };
        let w::Event {
            payload: Some(Payload::Snapshot(snapshot)),
            ..
        } = first.to_owned_message()
        else {
            anyhow::bail!("host did not send an initial snapshot");
        };
        let mut activations = snapshot.activations.clone();
        for companion in &snapshot.companions {
            if !activations
                .iter()
                .any(|active| active.companion_id == companion.id)
            {
                activations.push(activate(&clients, &registered.id, &companion.id, true).await?);
            }
        }
        let session = self.stop.child_token();
        let _close = session.clone().drop_guard();
        let (selected, activated) = watch::channel(activations);
        let (speaking, _) = watch::channel(*speaker);
        let (eye_enabled, eye_state) = watch::channel(self.device.eyes_enabled);
        let (plugins_changed, plugins) = watch::channel(self.plugins.clone());
        let mut frames = HashMap::<SubscriptionId, Option<crate::computer::Frame>>::new();
        let mut capture_scope = session.child_token();
        let mut capture_job = None::<tokio::task::AbortHandle>;
        let mut workers = tokio::task::JoinSet::new();
        workers.spawn(audio_worker(
            clients.clone(),
            registered.id.clone(),
            speaking.subscribe(),
            updates.clone(),
            session.clone(),
        ));
        workers.spawn(observe_enabled(
            clients.clone(),
            registered.id.clone(),
            activated.clone(),
            eye_state,
            plugins,
            session.clone(),
        ));
        let mut operations = clients
            .device
            .operations(subscription(&registered.id, 0))
            .await?;
        if let Some(renderer) = &self.renderer {
            let _result = renderer
                .sync(&snapshot, installed, &self.assets, local, self.avatar_scale)
                .await;
        }
        updates.send(Update::Connection(true)).await?;
        updates
            .send(Update::Event(first.to_owned_message()))
            .await?;
        loop {
            tokio::select! {
                () = session.cancelled() => return Ok(()),
                req = requests.recv() => {
                    let Some(req) = req else { return Ok(()); };
                    match req {
                        Action::AvatarScale(scale) => {
                            if let Some(renderer) = &self.renderer {
                                for character in installed.keys() {
                                    let result = renderer.command(Body::Resize {
                                        character: character.clone(), size_percent: scale,
                                    }).await;
                                    if !matches!(result, Ok(true)) {
                                        let _closed = updates.send(Update::Notice("アバターのサイズを変更できませんでした。".into())).await;
                                    }
                                }
                                self.avatar_scale = scale;
                                updates.send(Update::AvatarScale(scale)).await?;
                            }
                        }
                        Action::ResetHistory => {
                            if self.local_host.is_some() && local {
                                clients.settings.reset_history(w::Empty::default()).await?;
                            }
                        }
                        Action::Activate(companion) => {
                            let activation = activate(&clients, &registered.id, &companion, false).await?;
                            selected.send_if_modified(|active| upsert_activation(active, activation));
                        }
                        Action::Eyes(eyes) => {
                            self.device.eyes_enabled = eyes;
                            let changed = clients.device.register_device(w::DeviceChange {
                                mutation: mutation(0), device: Some(self.device.clone()).into(), ..Default::default()
                            }).await;
                            eye_enabled.send_replace(eyes && changed.is_ok());
                            if let Err(error) = changed {
                                updates.send(Update::Notice(error.message.unwrap_or_else(|| "操作に失敗しました。".into()))).await?;
                            }
                        }
                        Action::ClientPlugin { path, enabled } => {
                            let mut paths = self.plugin_paths.clone();
                            paths.retain(|item| *item != path);
                            if enabled { paths.push(path); }
                            let candidate = paths.clone();
                            let loaded = tokio::task::spawn_blocking(move || crate::plugins::ClientPlugin::load(&candidate)).await?;
                            let Ok(loaded) = loaded else {
                                updates.send(Update::Notice("端末プラグインの追加・削除を適用できません。".into())).await?;
                                continue;
                            };
                            let mut device = self.device.clone();
                            device.capabilities.retain(|cap| !cap.name.starts_with("body.plugin."));
                            if self.renderer.is_some() {
                                device.capabilities.extend(loaded.iter().filter_map(crate::plugins::ClientPlugin::capability));
                            }
                            clients.device.register_device(w::DeviceChange {
                                mutation: mutation(0), device: Some(device.clone()).into(), ..Default::default()
                            }).await?;
                            self.device = device;
                            self.plugin_paths = paths;
                            self.plugins = loaded.into();
                            plugins_changed.send_replace(self.plugins.clone());
                            updates.send(Update::ClientPlugins(self.plugin_paths.clone())).await?;
                        }
                        Action::Workspace(path) => {
                            match crate::workspace::Workspace::open(&path) {
                                Ok(workspace) => {
                                    let mut device = self.device.clone();
                                    device.capabilities.retain(|cap| !cap.name.starts_with(crate::workspace::PREFIX));
                                    device.capabilities.extend(workspace.specs());
                                    clients.device.register_device(w::DeviceChange {
                                        mutation: mutation(0), device: Some(device.clone()).into(), ..Default::default()
                                    }).await?;
                                    self.device = device;
                                    updates.send(Update::Workspace(workspace.path().to_owned())).await?;
                                    self.workspace = Some(std::sync::Arc::new(workspace));
                                }
                                Err(_) => updates.send(Update::Notice("作業フォルダーを開けません。".into())).await?,
                            }
                        }
                        Action::Input(enabled) => {
                            frames.clear();
                            capture_scope.cancel();
                            capture_scope = session.child_token();
                            self.device.capabilities.retain(|cap| cap.name != "desktop.input");
                            if enabled && crate::computer::available() {
                                self.device.capabilities.push(crate::computer::capability());
                            }
                            clients.device.register_device(w::DeviceChange {
                                mutation: mutation(0), device: Some(self.device.clone()).into(), ..Default::default()
                            }).await?;
                        }
                        Action::Speaker(yes) => {
                            *speaker = yes;
                            speaking.send_replace(yes);
                        }
                        req => {
                            let clients = clients.clone();
                            let updates = updates.clone();
                            let activation = match &req {
                                Action::Capture(generation, ..) | Action::Interrupt(generation) => activated
                                    .borrow().iter().find(|active| active.generation == *generation).cloned(),
                                _ => None,
                            }.unwrap_or_default();
                            let device = registered.id.clone();
                            let setup = matches!(req, Action::SetupProvider(..) | Action::SetupModel(..));
                            workers.spawn(async move {
                                if let Err(error) = call(clients, updates.clone(), &device, activation, req).await {
                                    let message = error.downcast_ref::<connectrpc::ConnectError>()
                                        .and_then(|error| error.message.as_deref()).unwrap_or("操作に失敗しました。").to_owned();
                                    let update = if setup { Update::SetupFailed(message) } else { Update::Notice(message) };
                                    let _closed = updates.send(update).await;
                                } else if setup {
                                    let _closed = updates.send(Update::SetupFinished).await;
                                }
                            });
                        }
                    }
                }
                event = stream.message::<w::Event>() => {
                    let event = event?.context("host subscription closed")?.to_owned_message();
                    match event.payload.as_ref() {
                        Some(Payload::Activation(activation)) => {
                            selected.send_if_modified(|active| upsert_activation(active, (**activation).clone()));
                        }
                        Some(Payload::Snapshot(snapshot)) => {
                            selected.send_if_modified(|active| {
                                if *active == snapshot.activations { return false; }
                                active.clone_from(&snapshot.activations);
                                true
                            });
                            if let Some(renderer) = &self.renderer {
                                let _result = renderer.sync(snapshot, installed, &self.assets, local, self.avatar_scale).await;
                            }
                        }
                        _ => {}
                    }
                    frames.retain(|subscription, _| activated.borrow().iter().any(|active| {
                        SubscriptionId::from_generation(active.generation) == Some(*subscription) && active.device_id == registered.id
                    }));
                    updates.send(Update::Event(event)).await?;
                }
                operation = operations.message::<w::DeviceOperation>() => {
                    let operation = operation?.context("device stream closed")?.to_owned_message();
                    let generation = operation.generation;
                    let claim = clients.device.claim(w::DeviceClaim {
                        device_id: registered.id.clone(), generation, operation_id: operation.id, ..Default::default()
                    }).await;
                    let Ok(claim) = claim else { continue; };
                    let command = claim.into_owned();
                    if command.capability == "desktop.input" && crate::computer::portal_session() {
                        let enabled = self.device.capabilities.iter().any(|cap| cap.name == command.capability);
                        if !enabled || capture_job.as_ref().is_some_and(|job| !job.is_finished()) {
                            let _stale = clients.device.complete(device_result(command, &registered.id, Err(anyhow::anyhow!("capture unavailable")))).await;
                            continue;
                        }
                        let cancel = capture_scope.clone();
                        let mut active = activated.clone();
                        let clients = clients.clone();
                        let device = registered.id.clone();
                        capture_job = Some(workers.spawn(async move {
                            let result = tokio::select! {
                                biased;
                                () = cancel.cancelled() => Err(anyhow::anyhow!("capture cancelled")),
                                _ = active.wait_for(|entries| !entries.iter().any(|active| active.generation == generation)) => Err(anyhow::anyhow!("activation changed")),
                                result = async {
                                    let args = serde_json::from_str(&command.arguments_json)?;
                                    crate::computer::execute(&mut None, command.run_id.clone(), args).await
                                } => result,
                            };
                            let _stale = clients.device.complete(device_result(command, &device, result)).await;
                        }));
                        continue;
                    }
                    let Some(subscription) = SubscriptionId::from_generation(generation) else {
                        continue;
                    };
                    let frame = frames.entry(subscription).or_default();
                    let result = self.execute(command, &registered.id, frame).await;
                    let _stale = clients.device.complete(result).await;
                }
                _ = workers.join_next(), if !workers.is_empty() => {}
            }
        }
    }
}

struct Session<'a> {
    clients: Clients,
    updates: &'a mpsc::Sender<Update>,
    requests: &'a mut mpsc::Receiver<Action>,
    installed: &'a mut HashMap<String, String>,
    local: bool,
    speaker: &'a mut bool,
}

fn upsert_activation(active: &mut Vec<w::Activation>, entry: w::Activation) -> bool {
    if let Some(previous) = active
        .iter_mut()
        .find(|current| current.companion_id == entry.companion_id)
    {
        if *previous == entry {
            return false;
        }
        *previous = entry;
    } else {
        active.push(entry);
        active.sort_by(|left, right| left.companion_id.cmp(&right.companion_id));
    }
    true
}

async fn activate(
    clients: &Clients,
    device: &str,
    companion: &str,
    only_if_unassigned: bool,
) -> Result<w::Activation> {
    Ok(clients
        .device
        .activate(w::ActivationRequest {
            mutation: mutation(0),
            device_id: device.into(),
            companion_id: companion.into(),
            only_if_unassigned,
            ..Default::default()
        })
        .await?
        .into_owned())
}

pub(crate) fn mutation(
    revision: i64,
) -> buffa::MessageField<w::Mutation, buffa::Inline<w::Mutation>> {
    Some(w::Mutation {
        request_key: id(),
        revision,
        ..Default::default()
    })
    .into()
}

pub(crate) fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn subscription(device_id: &str, generation: u64) -> w::AudioSubscription {
    w::AudioSubscription {
        device_id: device_id.into(),
        generation,
        ..Default::default()
    }
}
