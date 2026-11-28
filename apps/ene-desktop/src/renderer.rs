use anyhow::{Context, Result};
use ene_core::avatar::{AvatarEvent, AvatarMessage, CharacterConfig, Command, Request};
use ene_rpc as w;
use process_wrap::tokio::CommandWrap;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot, watch},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct Renderer {
    requests: mpsc::Sender<(Request, oneshot::Sender<bool>)>,
    lips: watch::Sender<(String, f32)>,
    next: Arc<AtomicU64>,
}

impl Renderer {
    pub(crate) fn spawn(
        path: &Path,
        layout: &Path,
        stop: CancellationToken,
        desktop_events: mpsc::Sender<crate::tray::Action>,
    ) -> Result<Self> {
        let mut command = tokio::process::Command::new(path);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .env(ene_core::avatar::LAYOUT_FILE_ENV, layout);
        // The DX12 visual swapchain preserves transparency on the desktop.
        #[cfg(windows)]
        command.env("WGPU_DX12_PRESENTATION_SYSTEM", "Visual");
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::tokio::JobObject);
        command.wrap(process_wrap::tokio::KillOnDrop);
        let mut child = command.spawn()?;
        let mut input = child
            .inner_mut()
            .stdin()
            .take()
            .context("renderer stdin unavailable")?;
        let output = child
            .inner_mut()
            .stdout()
            .take()
            .context("renderer stdout unavailable")?;
        let (requests, mut work) = mpsc::channel::<(Request, oneshot::Sender<bool>)>(32);
        let (lips, mut amplitude) = watch::channel((String::new(), 0.0_f32));
        tokio::spawn(async move {
            let mut lines = BufReader::new(output).lines();
            let mut replies = HashMap::<u64, oneshot::Sender<bool>>::new();
            loop {
                let request = tokio::select! {
                    () = stop.cancelled() => break,
                    command = work.recv() => match command {
                        Some((request, reply)) => {
                            if let Some(id) = request.id { replies.insert(id, reply); }
                            request
                        }
                        None => break,
                    },
                    value = amplitude.changed() => {
                        if value.is_err() { break; }
                        let (character, rms) = amplitude.borrow_and_update().clone();
                        if character.is_empty() { continue; }
                        Request { id: None, command: Command::LipSync { character, rms } }
                    },
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break; };
                        let Ok(message) = serde_json::from_str::<AvatarMessage>(&line) else { continue; };
                        match message {
                            AvatarMessage::Reply { id, .. } => { if let Some(reply) = replies.remove(&id) { let _closed = reply.send(true); } }
                            AvatarMessage::ErrorReply { id, .. } => { if let Some(reply) = replies.remove(&id) { let _closed = reply.send(false); } }
                            AvatarMessage::Fatal { .. } => break,
                            AvatarMessage::Event(AvatarEvent::Click { character }) => {
                                let _busy = desktop_events.try_send(crate::tray::Action::Select(character));
                            }
                            AvatarMessage::Event(_) => {}
                        }
                        continue;
                    },
                };
                replies.retain(|_, reply| !reply.is_closed());
                let Ok(line) = request.encode_line() else {
                    continue;
                };
                if input.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            drop(input);
            if tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .is_err()
            {
                let _reaped = Box::into_pin(child.kill()).await;
            }
        });
        Ok(Self {
            requests,
            lips,
            next: Arc::new(AtomicU64::new(1)),
        })
    }

    pub(crate) async fn command(&self, command: Command) -> Result<bool> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (reply, wait) = oneshot::channel();
        self.requests
            .send((
                Request {
                    id: Some(id),
                    command,
                },
                reply,
            ))
            .await
            .context("renderer closed")?;
        Ok(tokio::time::timeout(Duration::from_secs(8), wait).await??)
    }

    pub(crate) fn lip(&self, companion: String, rms: f32) {
        self.lips.send_replace((companion, rms));
    }

    pub(crate) async fn sync(
        &self,
        snapshot: &w::Snapshot,
        installed: &mut HashMap<String, String>,
        assets: &Path,
        local: bool,
        avatar_scale: f32,
    ) -> Result<()> {
        let mut live = HashSet::new();
        for companion in &snapshot.companions {
            let Some(character) = snapshot
                .characters
                .iter()
                .find(|item| item.id == companion.character_id)
            else {
                continue;
            };
            if character.avatar.is_empty() {
                continue;
            }
            let candidate = PathBuf::from(&character.avatar);
            let model = if candidate == Path::new("assets/ENE.vrm") {
                assets.join("ENE.vrm")
            } else if local {
                candidate
            } else {
                continue;
            };
            let Ok(model) = model.canonicalize() else {
                continue;
            };
            live.insert(companion.id.clone());
            let model = model.to_string_lossy().into_owned();
            if installed.get(&companion.id) == Some(&model) {
                continue;
            }
            let motion = assets
                .join("vrma/idle-breath.vrma")
                .canonicalize()
                .ok()
                .map(|path| path.to_string_lossy().into_owned());
            let config = CharacterConfig {
                model: model.clone(),
                motion,
                size_percent: avatar_scale,
                ..Default::default()
            };
            let id = companion.id.clone();
            let command = if installed.contains_key(&id) {
                Command::Update {
                    character: id.clone(),
                    config,
                }
            } else {
                Command::Add {
                    character: id.clone(),
                    config,
                }
            };
            if self.command(command).await? {
                installed.insert(id, model);
            }
        }
        let removed: Vec<_> = installed
            .keys()
            .filter(|key| !live.contains(*key))
            .cloned()
            .collect();
        for id in removed {
            if self
                .command(Command::Remove {
                    character: id.clone(),
                })
                .await?
            {
                installed.remove(&id);
            }
        }
        Ok(())
    }
}
