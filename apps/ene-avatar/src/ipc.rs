//! Remoc service owning native windows and state/event channels.
use ene_ipc::renderer::{
    AvatarConfig, AvatarEvent, AvatarId, AvatarInstance, AvatarState, Renderer, RendererConfig,
    RendererError, RendererMessage, RendererState,
};
#[cfg(unix)]
use smithay_client_toolkit::reexports::calloop::channel;
#[cfg(windows)]
mod channel {
    pub use std::sync::mpsc::{SyncSender, sync_channel};
    pub type Channel<T> = std::sync::mpsc::Receiver<T>;
}
use remoc::rch::watch;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

type Result = std::result::Result<(), RendererError>;

#[derive(Clone)]
pub struct Output {
    pub messages: Option<futures_channel::mpsc::UnboundedSender<RendererMessage>>,
    pub states: Arc<watch::Sender<RendererState>>,
}
impl Output {
    pub fn send(&self, message: RendererMessage) {
        if let Some(sender) = &self.messages {
            let _ = sender.unbounded_send(message);
        }
    }
    pub fn error(&self, avatar_id: Option<AvatarId>, message: impl Into<String>) {
        self.send(RendererMessage::Error {
            avatar_id,
            message: message.into(),
        });
    }
    pub fn update_state(&self, id: AvatarId, update: impl FnOnce(&mut AvatarState)) {
        self.states.send_modify(|states| {
            if let Some(state) = states.avatars.get_mut(&id) {
                update(state);
            }
        });
    }
    pub fn avatar_error(&self, id: AvatarId, message: impl Into<String>) {
        self.update_state(id, |state| {
            state.error = Some(message.into());
            state.loading = false;
        });
    }
    pub fn emit(&self, id: AvatarId, event: AvatarEvent) {
        self.send(RendererMessage::Event {
            avatar_id: id,
            event,
        });
    }
}

pub enum WindowRequest {
    Configure(AvatarConfig),
    RenderConfig(RendererConfig),
    Recenter,
}
pub struct WindowIpc {
    pub requests: channel::Channel<WindowRequest>,
    pub output: Output,
}
type AvatarRegistration = scopeguard::ScopeGuard<(AvatarId, Output), fn((AvatarId, Output))>;

struct Window {
    id: AvatarId,
    // Fields drop in declaration order: close requests, join, then unregister.
    requests: channel::SyncSender<WindowRequest>,
    _worker: jod_thread::JoinHandle,
    _registration: AvatarRegistration,
}
impl Window {
    fn new(avatar: AvatarInstance, fps: u32, output: Output) -> Self {
        let id = avatar.id;
        let (requests, receiver) = channel::sync_channel(64);
        output.states.send_modify(|states| {
            states.avatars.insert(
                id,
                AvatarState {
                    placement: avatar.placement.clone(),
                    ..Default::default()
                },
            );
        });
        let ipc = WindowIpc {
            requests: receiver,
            output: output.clone(),
        };
        let worker_output = output.clone();
        let worker = jod_thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::application::run_window(id, avatar.config, avatar.placement, fps, ipc)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => worker_output.avatar_error(id, error.to_string()),
                Err(_) => worker_output.avatar_error(
                    id,
                    "Avatar rendering failed; remove and add the avatar to retry.",
                ),
            }
        });
        Self {
            id,
            requests,
            _worker: worker,
            _registration: scopeguard::guard((id, output), |(id, output)| {
                output.states.send_modify(|states| {
                    states.avatars.remove(&id);
                });
            }),
        }
    }
    fn command(&self, request: WindowRequest) -> Result {
        self.requests.try_send(request).map_err(|_| {
            RendererError::Rejected("Avatar command queue full or window stopped.".into())
        })
    }
}

pub struct RendererService {
    windows: HashMap<AvatarId, Window>,
    output: Output,
}
impl RendererService {
    pub fn new(output: Output) -> Self {
        Self {
            windows: HashMap::new(),
            output,
        }
    }
    fn config(&self) -> std::result::Result<RendererConfig, RendererError> {
        self.output
            .states
            .borrow()
            .config
            .ok_or_else(|| RendererError::Rejected("Renderer must be initialized first.".into()))
    }
    fn window(&self, id: AvatarId) -> std::result::Result<&Window, RendererError> {
        self.config()?;
        self.windows
            .get(&id)
            .ok_or_else(|| RendererError::Rejected("Unknown avatar ID.".into()))
    }
}
impl Renderer for RendererService {
    async fn initialize(&mut self, config: RendererConfig, avatars: Vec<AvatarInstance>) -> Result {
        if self.output.states.borrow().config.is_some() {
            return Err(RendererError::Rejected(
                "Renderer is already initialized.".into(),
            ));
        }
        let mut ids = HashSet::with_capacity(avatars.len());
        for avatar in &avatars {
            avatar.config.validate().map_err(RendererError::Rejected)?;
            if !ids.insert(avatar.id) {
                return Err(RendererError::Rejected(format!(
                    "Duplicate avatar ID: {}",
                    avatar.id
                )));
            }
        }
        for avatar in avatars {
            self.windows.insert(
                avatar.id,
                Window::new(avatar, config.fps, self.output.clone()),
            );
        }
        self.output
            .states
            .send_modify(|state| state.config = Some(config));
        Ok(())
    }
    async fn set_render_config(&mut self, config: RendererConfig) -> Result {
        if self.config()? != config {
            for window in self.windows.values() {
                if let Err(error) = window.command(WindowRequest::RenderConfig(config)) {
                    self.output.error(Some(window.id), error.to_string());
                }
            }
            self.output
                .states
                .send_modify(|state| state.config = Some(config));
        }
        Ok(())
    }
    async fn add_avatar(&mut self, avatar: AvatarInstance) -> Result {
        let fps = self.config()?.fps;
        avatar.config.validate().map_err(RendererError::Rejected)?;
        if self.windows.contains_key(&avatar.id) {
            return Err(RendererError::Rejected("Avatar ID already exists.".into()));
        }
        self.windows
            .insert(avatar.id, Window::new(avatar, fps, self.output.clone()));
        Ok(())
    }
    async fn remove_avatar(&mut self, avatar_id: AvatarId) -> Result {
        self.window(avatar_id)?;
        self.windows.remove(&avatar_id);
        Ok(())
    }
    async fn set_avatar_config(&mut self, avatar_id: AvatarId, config: AvatarConfig) -> Result {
        config.validate().map_err(RendererError::Rejected)?;
        self.window(avatar_id)?
            .command(WindowRequest::Configure(config))
    }
    async fn recenter(&mut self, avatar_id: AvatarId) -> Result {
        self.window(avatar_id)?.command(WindowRequest::Recenter)
    }
    async fn shutdown(self) -> Result {
        // The consuming RPC stops the generated server and releases all windows.
        drop(self);
        Ok(())
    }
}
