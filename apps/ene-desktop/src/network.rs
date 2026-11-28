use crate::renderer::Renderer;
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

#[derive(Deserialize)]
struct Discovery {
    endpoint: String,
    token: String,
}

#[derive(Clone)]
struct Clients {
    state: w::HostServiceClient<HttpClient>,
    chat: w::ChatServiceClient<HttpClient>,
    work: w::WorkServiceClient<HttpClient>,
    settings: w::SettingsServiceClient<HttpClient>,
    device: w::ClientServiceClient<HttpClient>,
    voice: w::VoiceServiceClient<HttpClient>,
}

impl Clients {
    fn connect(file: &Path) -> Result<(Self, bool)> {
        let handle = std::fs::File::open(file)?;
        private_fs::verify(&handle)?;
        let Discovery { endpoint, token } = serde_json::from_reader(handle)?;
        let local = endpoint.starts_with("http://");
        let (transport, config) = if local {
            w::connect(&endpoint, &token)?
        } else {
            w::connect_remote(&endpoint, &token)?
        };
        Ok((Self::new(transport, config), local))
    }

    fn new(transport: HttpClient, config: ClientConfig) -> Self {
        Self {
            state: w::HostServiceClient::new(
                transport.clone(),
                config
                    .clone()
                    .with_default_max_message_size(48 * 1024 * 1024),
            ),
            chat: w::ChatServiceClient::new(transport.clone(), config.clone()),
            work: w::WorkServiceClient::new(transport.clone(), config.clone()),
            settings: w::SettingsServiceClient::new(transport.clone(), config.clone()),
            device: w::ClientServiceClient::new(transport.clone(), config.clone()),
            voice: w::VoiceServiceClient::new(transport, config),
        }
    }
}

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
}

#[derive(Clone)]
pub(crate) enum Action {
    History(String),
    Send(w::SendRequest),
    Stop(w::StopRequest),
    StopWork(w::StopRequest),
    Answer(w::ApprovalAnswer),
    Settings,
    ClientPlugin { path: PathBuf, enabled: bool },
    SetupProvider(w::Connection, Option<String>),
    SetupModel(w::Companion, w::ModelSelection),
    PutConnection(w::ConnectionChange),
    DeleteConnection(w::DeleteRequest),
    Credential(w::CredentialChange),
    Models(String),
    Probe(w::ProbeRequest),
    Policy(w::PolicyChange),
    DeleteRule(w::DeleteRequest),
    PutPlugin(w::PluginChange),
    DeletePlugin(w::DeleteRequest),
    PutCharacter(w::CharacterChange),
    PutCompanion(w::CompanionChange),
    Preview(w::PreviewRequest),
    Import(PathBuf),
    Export(w::ExportRequest, PathBuf),
    Activate,
    Eyes(bool),
    Input(bool),
    Speaker(bool),
    Interrupt,
    Capture(Vec<u8>, u32),
}

pub(crate) enum Update {
    Connection(bool),
    Event(w::Event),
    History(String, Vec<w::Message>),
    Settings(w::Settings),
    ClientPlugins(Vec<PathBuf>),
    Models(Vec<String>),
    Audit(Vec<w::AuditEntry>),
    Audio(w::AudioOutput),
    Notice(String),
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
        let activation = if snapshot.activation.device_id.is_empty() {
            activate(&clients, &registered.id).await?
        } else {
            (*snapshot.activation).clone()
        };
        let session = self.stop.child_token();
        let _close = session.clone().drop_guard();
        let (selected, activated) = watch::channel(activation);
        let (speaking, _) = watch::channel(*speaker);
        let (eye_enabled, eye_state) = watch::channel(self.device.eyes_enabled);
        let (plugins_changed, plugins) = watch::channel(self.plugins.clone());
        let mut frame = None;
        let mut capture_scope = session.child_token();
        let mut capture_job = None::<tokio::task::AbortHandle>;
        let mut workers = tokio::task::JoinSet::new();
        workers.spawn(audio_worker(
            clients.clone(),
            registered.id.clone(),
            activated.clone(),
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
        let mut operations = None;
        let mut current = activated.borrow().generation;
        if activated.borrow().device_id == registered.id {
            operations = Some(
                clients
                    .device
                    .operations(subscription(&registered.id, current))
                    .await?,
            );
        }
        if let Some(renderer) = &self.renderer {
            let _result = renderer
                .sync(&snapshot, installed, &self.assets, local)
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
                        Action::Activate => {
                            let activation = activate(&clients, &registered.id).await?;
                            selected.send_replace(activation);
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
                        Action::Input(enabled) => {
                            frame = None;
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
                            let activation = activated.borrow().clone();
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
                    let changed = match event.payload.as_ref() {
                        Some(Payload::Activation(activation)) => Some((**activation).clone()),
                        Some(Payload::Snapshot(snapshot)) => {
                            if let Some(renderer) = &self.renderer {
                                let _result = renderer.sync(snapshot, installed, &self.assets, local).await;
                            }
                            snapshot.activation.as_option().cloned()
                        }
                        _ => None,
                    };
                    if let Some(activation) = changed {
                        if activation.generation != current {
                            frame = None;
                            capture_scope.cancel();
                            capture_scope = session.child_token();
                            current = activation.generation;
                            operations = if activation.device_id == registered.id {
                                Some(clients.device.operations(subscription(&registered.id, current)).await?)
                            } else { None };
                        }
                        selected.send_replace(activation);
                    }
                    updates.send(Update::Event(event)).await?;
                }
                operation = async {
                    match &mut operations {
                        Some(stream) => stream.message::<w::DeviceOperation>().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let operation = operation?.context("device stream closed")?.to_owned_message();
                    if operation.generation != current { continue; }
                    let claim = clients.device.claim(w::DeviceClaim {
                        device_id: registered.id.clone(), generation: current, operation_id: operation.id, ..Default::default()
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
                        let clients = clients.clone();
                        let device = registered.id.clone();
                        capture_job = Some(workers.spawn(async move {
                            let result = tokio::select! {
                                biased;
                                () = cancel.cancelled() => Err(anyhow::anyhow!("capture cancelled")),
                                result = async {
                                    let args = serde_json::from_str(&command.arguments_json)?;
                                    crate::computer::execute(&mut None, command.run_id.clone(), args).await
                                } => result,
                            };
                            let _stale = clients.device.complete(device_result(command, &device, result)).await;
                        }));
                        continue;
                    }
                    let result = self.execute(command, &registered.id, &mut frame).await;
                    let _stale = clients.device.complete(result).await;
                }
                _ = workers.join_next(), if !workers.is_empty() => {}
            }
        }
    }

    async fn execute(
        &self,
        command: w::DeviceOperation,
        device: &str,
        frame: &mut Option<crate::computer::Frame>,
    ) -> w::DeviceResult {
        let result = async {
            let args: Value = serde_json::from_str(&command.arguments_json)?;
            if command.capability == "desktop.input" {
                ensure!(
                    self.device
                        .capabilities
                        .iter()
                        .any(|cap| cap.name == command.capability),
                    "desktop input disabled"
                );
                return crate::computer::execute(frame, command.run_id.clone(), args).await;
            }
            let renderer = self.renderer.as_ref().context("renderer unavailable")?;
            let name = args
                .get("name")
                .and_then(Value::as_str)
                .context("gesture name missing")?;
            let body = match command.capability.as_str() {
                "body.expression" => Body::Expression {
                    character: command.companion_id.clone(),
                    name: (name != "neutral").then(|| name.to_owned()),
                },
                "body.motion" => {
                    let file = match name {
                        "nod" => "acknowledge",
                        "wave" => "welcome-wave",
                        "listen" => "listen-active",
                        "bow" => "polite-bow",
                        "idle" => "idle-breath",
                        "sit" => "perch-idle",
                        other => other,
                    };
                    ensure!(
                        !file.is_empty()
                            && file
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
                        "invalid motion name"
                    );
                    let path = self
                        .assets
                        .join("vrma")
                        .join(format!("{file}.vrma"))
                        .canonicalize()?;
                    Body::Motion {
                        character: command.companion_id.clone(),
                        motion: Some(path.to_string_lossy().into_owned()),
                        looping: false,
                    }
                }
                capability => {
                    let plugin = self
                        .plugins
                        .iter()
                        .find(|plugin| plugin.serves_body(capability))
                        .context("capability unsupported")?;
                    plugin
                        .body(&command, device, args, &self.assets, &self.stop)
                        .await?
                }
            };
            ensure!(
                renderer.command(body).await?,
                "renderer rejected the request"
            );
            Ok(None)
        }
        .await;
        device_result(command, device, result)
    }
}

fn device_result(
    command: w::DeviceOperation,
    device: &str,
    result: Result<Option<w::ScreenFrame>>,
) -> w::DeviceResult {
    let (outcome, result_json, frame) = match result {
        Ok(frame) => (
            w::Outcome::Completed,
            frame
                .as_ref()
                .map_or_else(
                    || json!({"applied":true}),
                    |frame| json!({"frame_id":frame.id,"width":frame.width,"height":frame.height}),
                )
                .to_string(),
            frame,
        ),
        Err(_) => (w::Outcome::Failed, String::new(), None),
    };
    w::DeviceResult {
        device_id: device.into(),
        generation: command.generation,
        operation_id: command.id,
        outcome: outcome.into(),
        result_json,
        frame: frame.into(),
        ..Default::default()
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

async fn activate(clients: &Clients, device: &str) -> Result<w::Activation> {
    Ok(clients
        .device
        .activate(w::ActivationRequest {
            mutation: mutation(0),
            device_id: device.into(),
            ..Default::default()
        })
        .await?
        .into_owned())
}

async fn call(
    clients: Clients,
    updates: mpsc::Sender<Update>,
    device: &str,
    active: w::Activation,
    action: Action,
) -> Result<()> {
    let mut settings = false;
    match action {
        Action::History(companion) => {
            let history = clients
                .chat
                .get_history(w::HistoryRequest {
                    companion_id: companion.clone(),
                    limit: 100,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates
                .send(Update::History(companion, history.messages))
                .await?;
        }
        Action::Send(query) => {
            clients.chat.send(query).await?;
        }
        Action::Stop(query) => {
            clients.chat.stop(query).await?;
        }
        Action::StopWork(query) => {
            clients.work.stop(query).await?;
        }
        Action::Answer(query) => {
            clients.settings.answer(query).await?;
        }
        Action::Settings => {
            settings = true;
            let audit = clients
                .settings
                .audit_history(w::AuditQuery {
                    limit: 50,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates.send(Update::Audit(audit.items)).await?;
        }
        Action::SetupProvider(connection, secret) => {
            let saved = if connection.revision == 0 {
                clients
                    .settings
                    .put_connection(w::ConnectionChange {
                        mutation: mutation(0),
                        connection: Some(connection).into(),
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
            } else {
                connection
            };
            if let Some(secret) = secret {
                let credential = clients
                    .settings
                    .set_credential(w::CredentialChange {
                        mutation: mutation(saved.revision),
                        connection_id: saved.id.clone(),
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await;
                if let Err(error) = credential {
                    updates
                        .send(Update::Settings(
                            clients
                                .settings
                                .get(w::Empty::default())
                                .await?
                                .into_owned(),
                        ))
                        .await?;
                    return Err(error.into());
                }
            }
            updates
                .send(Update::Settings(
                    clients
                        .settings
                        .get(w::Empty::default())
                        .await?
                        .into_owned(),
                ))
                .await?;
            let models = clients
                .settings
                .models(w::Id {
                    id: saved.id,
                    ..Default::default()
                })
                .await;
            match models {
                Ok(models) => {
                    updates
                        .send(Update::Models(models.into_owned().ids))
                        .await?
                }
                Err(err)
                    if err.code == connectrpc::ErrorCode::Unauthenticated
                        || err.code == connectrpc::ErrorCode::ResourceExhausted =>
                {
                    return Err(err.into());
                }
                Err(_) => updates.send(Update::Models(Vec::new())).await?,
            }
        }
        Action::SetupModel(mut companion, model) => {
            clients
                .settings
                .probe(w::ProbeRequest {
                    connection_id: model.connection_id.clone(),
                    model: Some(model.clone()).into(),
                    ..Default::default()
                })
                .await?;
            companion
                .models
                .retain(|selection| selection.role != model.role);
            for role in [
                w::model_selection::Role::Work,
                w::model_selection::Role::Policy,
            ] {
                if companion.models.iter().all(|item| item.role != role) {
                    companion.models.push(w::ModelSelection {
                        role: role.into(),
                        ..model.clone()
                    });
                }
            }
            companion.models.push(model);
            clients
                .state
                .put_companion(w::CompanionChange {
                    mutation: mutation(companion.revision),
                    companion: Some(companion).into(),
                    ..Default::default()
                })
                .await?;
        }
        Action::PutConnection(query) => {
            clients.settings.put_connection(query).await?;
            settings = true;
        }
        Action::DeleteConnection(query) => {
            clients.settings.delete_connection(query).await?;
            settings = true;
        }
        Action::Credential(query) => {
            clients.settings.set_credential(query).await?;
            settings = true;
        }
        Action::Models(id) => {
            let models = clients
                .settings
                .models(w::Id {
                    id,
                    ..Default::default()
                })
                .await?
                .into_owned();
            updates.send(Update::Models(models.ids)).await?;
        }
        Action::Probe(query) => {
            clients.settings.probe(query).await?;
            updates
                .send(Update::Notice("接続を確認しました。".into()))
                .await?;
        }
        Action::Policy(query) => {
            clients.settings.put_policy(query).await?;
            settings = true;
        }
        Action::DeleteRule(query) => {
            clients.settings.delete_rule(query).await?;
            settings = true;
        }
        Action::PutPlugin(query) => {
            clients.settings.put_plugin(query).await?;
            settings = true;
        }
        Action::DeletePlugin(query) => {
            clients.settings.delete_plugin(query).await?;
            settings = true;
        }
        Action::PutCharacter(query) => {
            clients.state.put_character(query).await?;
        }
        Action::PutCompanion(query) => {
            clients.state.put_companion(query).await?;
        }
        Action::Preview(query) => {
            let preview = clients.state.preview(query).await?.into_owned();
            updates.send(Update::Notice(preview.text)).await?;
        }
        Action::Import(path) => {
            ensure!(
                tokio::fs::metadata(&path).await?.len() <= 48 * 1024 * 1024,
                "character file too large"
            );
            let package = serde_json::from_slice(&tokio::fs::read(path).await?)?;
            clients
                .state
                .import_character(w::ImportRequest {
                    mutation: mutation(0),
                    package: Some(package).into(),
                    ..Default::default()
                })
                .await?;
        }
        Action::Export(request, path) => {
            let package = clients.state.export_character(request).await?.into_owned();
            save_new(path, &package).await?;
            updates
                .send(Update::Notice(if package.warnings.is_empty() {
                    "書き出しました。".into()
                } else {
                    package.warnings.join("\n")
                }))
                .await?;
        }
        Action::Capture(pcm, rate) => {
            ensure!(active.device_id == device, "another device is active");
            let pieces = pcm
                .chunks(16384)
                .map(|chunk| w::AudioInput {
                    device_id: device.into(),
                    generation: active.generation,
                    sample_rate: rate,
                    pcm: chunk.to_vec(),
                    ..Default::default()
                })
                .chain(std::iter::once(w::AudioInput {
                    device_id: device.into(),
                    generation: active.generation,
                    sample_rate: rate,
                    commit: true,
                    request_key: id(),
                    ..Default::default()
                }))
                .collect::<Vec<_>>();
            let mut stream = clients.voice.input().await?;
            for piece in pieces {
                stream.send(piece).await?;
            }
            stream.close_send();
            while stream.message::<w::Transcription>().await?.is_some() {}
        }
        Action::Interrupt => {
            if active.device_id == device {
                let mut stream = clients.voice.input().await?;
                stream
                    .send(w::AudioInput {
                        device_id: device.into(),
                        generation: active.generation,
                        interrupt: true,
                        ..Default::default()
                    })
                    .await?;
                stream.close_send();
                while stream.message::<w::Transcription>().await?.is_some() {}
            }
        }
        Action::Activate
        | Action::Eyes(_)
        | Action::Input(_)
        | Action::Speaker(_)
        | Action::ClientPlugin { .. } => {}
    }
    if settings {
        updates
            .send(Update::Settings(
                clients
                    .settings
                    .get(w::Empty::default())
                    .await?
                    .into_owned(),
            ))
            .await?;
    }
    Ok(())
}

async fn save_new(path: PathBuf, package: &impl Serialize) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let bytes = serde_json::to_vec(package)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(&bytes).await?;
    file.sync_all().await?;
    Ok(())
}

async fn audio_worker(
    clients: Clients,
    device: String,
    mut active: watch::Receiver<w::Activation>,
    mut enabled: watch::Receiver<bool>,
    updates: mpsc::Sender<Update>,
    stop: CancellationToken,
) {
    loop {
        let yes = *enabled.borrow_and_update();
        let activation = active.borrow_and_update().clone();
        if yes && activation.device_id == device {
            let stream = async {
                let mut stream = clients
                    .voice
                    .output(subscription(&device, activation.generation))
                    .await?;
                while let Some(message) = stream.message::<w::AudioOutput>().await? {
                    if updates
                        .send(Update::Audio(message.to_owned_message()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok::<_, connectrpc::ConnectError>(())
            };
            tokio::select! {
                () = stop.cancelled() => break,
                changed = active.changed() => { if changed.is_err() { break; } }
                changed = enabled.changed() => { if changed.is_err() { break; } }
                _ = stream => tokio::time::sleep(Duration::from_millis(500)).await,
            }
            let _closed = updates
                .send(Update::Audio(w::AudioOutput {
                    stop: true,
                    ..Default::default()
                }))
                .await;
        } else {
            tokio::select! {
                () = stop.cancelled() => break,
                changed = active.changed() => { if changed.is_err() { break; } }
                changed = enabled.changed() => { if changed.is_err() { break; } }
            }
        }
    }
}

async fn observe_enabled(
    clients: Clients,
    device: String,
    mut active: watch::Receiver<w::Activation>,
    mut enabled: watch::Receiver<bool>,
    mut plugins: watch::Receiver<std::sync::Arc<[crate::plugins::ClientPlugin]>>,
    stop: CancellationToken,
) {
    let mut previous = None;
    #[cfg(target_os = "linux")]
    let mut observer = None::<crate::observe_linux::Observer>;
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            changed = enabled.changed() => {
                if changed.is_err() { break; }
                previous = None;
                #[cfg(target_os = "linux")]
                { observer = None; }
            }
            changed = active.changed() => {
                if changed.is_err() { break; }
                previous = None;
            }
            changed = plugins.changed() => {
                if changed.is_err() { break; }
                previous = None;
            }
            () = tokio::time::sleep(Duration::from_secs(5)) => {
                let activation = active.borrow().clone();
                if !*enabled.borrow() || activation.device_id != device { continue; }
                #[cfg(target_os = "linux")]
                let observation = {
                    if observer.is_none() { observer = crate::observe_linux::Observer::open(); }
                    observer.as_mut().map(crate::observe_linux::Observer::observe)
                };
                #[cfg(windows)]
                let observation = Some(crate::observe::observe());
                #[cfg(not(any(windows,target_os = "linux")))]
                let observation = None::<crate::observe_types::Observation>;
                let observation = observation.unwrap_or_default();
                let crate::observe_types::ObservedWindow::Window(window) = observation.window else {
                    if previous.is_some() && clients.device.report_perception(w::Perception {
                        device_id: device.clone(), generation: activation.generation, ..Default::default()
                    }).await.is_ok() { previous = None; }
                    continue;
                };
                let idle = observation.idle_seconds.map(|idle| idle >= 120);
                let input = (window.app_id, window.title, idle);
                if previous.as_ref() == Some(&input) { continue; }
                let mut text = json!({"application":input.0, "idle":idle}).to_string();
                let mut interpretation = String::new();
                let loaded = plugins.borrow().clone();
                if let Some(plugin) = loaded.iter().find(|plugin| plugin.serves_perception()) {
                    let result = tokio::select! {
                        () = stop.cancelled() => break,
                        _ = active.changed() => { previous = None; continue; }
                        _ = enabled.changed() => { previous = None; continue; }
                        _ = plugins.changed() => { previous = None; continue; }
                        result = plugin.observe(&input.0, &input.1, idle, &stop) => result,
                    };
                    if let Ok((observed, inferred)) = result {
                        text = observed;
                        interpretation = inferred;
                    }
                }
                if !*enabled.borrow() || active.borrow().generation != activation.generation { continue; }
                if clients.device.report_perception(w::Perception {
                    device_id: device.clone(), generation: activation.generation, observation: text, interpretation, ..Default::default()
                }).await.is_ok() { previous = Some(input); }
            }
        }
    }
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

#[derive(Default)]
struct LocalHost {
    child: Option<std::process::Child>,
    attempted: Option<std::time::Instant>,
}

impl LocalHost {
    fn ensure_running(&mut self, executable: &Path, discovery: &Path) {
        if self
            .child
            .as_mut()
            .is_some_and(|process| matches!(process.try_wait(), Ok(None)))
            || self
                .attempted
                .is_some_and(|start| start.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.attempted = Some(std::time::Instant::now());
        let mut command = std::process::Command::new(executable);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        if let Some(runtime) = discovery.parent() {
            command.env("ENE_HOST_RUNTIME_DIR", runtime);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.spawn() {
            Ok(process) => self.child = Some(process),
            Err(_) => tracing::warn!("local host could not be started"),
        }
    }
}
