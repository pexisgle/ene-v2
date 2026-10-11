//! ECS-owned windows and native Remoc state/event channels.
use bevy::{app::AppExit, prelude::*};
use ene_ipc::{
    remoc::rch::{mpsc, watch},
    renderer::{
        AvatarConfig, AvatarEvent, AvatarId, AvatarInstance, AvatarState, RendererConfig,
        RendererMessage, RendererRequest, RendererState,
    },
};
#[cfg(unix)]
use smithay_client_toolkit::reexports::calloop::channel;
#[cfg(windows)]
mod channel {
    pub use std::sync::mpsc::{SyncSender, sync_channel};
    pub type Channel<T> = std::sync::mpsc::Receiver<T>;
}
use std::{sync::Arc, thread::JoinHandle};

#[derive(Resource, Clone)]
pub struct Output {
    pub messages: Option<mpsc::Sender<RendererMessage>>,
    pub states: Arc<watch::Sender<RendererState>>,
}
impl Output {
    pub fn send(&self, message: RendererMessage) {
        if let Some(sender) = &self.messages {
            let result = bevy::tasks::block_on(async {
                sender
                    .send(message)
                    .await
                    .map_err(|error| error.to_string())?
                    .await
                    .map_err(|error| error.to_string())
            });
            if let Err(error) = result {
                eprintln!("renderer notification: {error}");
            }
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
#[derive(Component)]
pub(crate) struct Window {
    id: AvatarId,
    requests: Option<channel::SyncSender<WindowRequest>>,
    worker: Option<JoinHandle<()>>,
    output: Output,
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
                    ..default()
                },
            );
        });
        let ipc = WindowIpc {
            requests: receiver,
            output: output.clone(),
        };
        let worker_output = output.clone();
        let worker = std::thread::spawn(move || {
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
            requests: Some(requests),
            worker: Some(worker),
            output,
        }
    }
    fn command(&self, request: WindowRequest) {
        if self.requests.as_ref().unwrap().try_send(request).is_err() {
            self.output.error(
                Some(self.id),
                "Avatar command queue full or window stopped.",
            );
        }
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.output.states.send_modify(|states| {
            states.avatars.remove(&self.id);
        });
        self.output
            .send(RendererMessage::AvatarRemoved { avatar_id: self.id });
    }
}

#[derive(Resource, PartialEq)]
pub(crate) struct Config(RendererConfig);

pub struct RendererPlugin(pub Output);
impl Plugin for RendererPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.0.clone())
            .add_systems(Update, apply_config);
    }
}

pub(crate) fn apply_request(
    In(request): In<RendererRequest>,
    mut commands: Commands,
    mut config: Option<ResMut<Config>>,
    windows: Query<(Entity, &Window)>,
    output: Res<Output>,
    mut exit: MessageWriter<AppExit>,
) {
    if let Err(message) = request.validate() {
        output.error(None, message);
        return;
    }
    if matches!(request, RendererRequest::Shutdown) {
        exit.write(AppExit::Success);
        return;
    }
    if let RendererRequest::Initialize {
        config: initial,
        avatars,
    } = request
    {
        if config.is_some() {
            output.error(None, "Renderer is already initialized.");
            return;
        }
        commands.insert_resource(Config(initial.clone()));
        for avatar in avatars {
            commands.spawn(Window::new(avatar, initial.fps, output.clone()));
        }
        return;
    }
    let Some(config) = &mut config else {
        output.error(None, "Renderer must be initialized first.");
        return;
    };
    let (id, request) = match request {
        RendererRequest::SetRenderConfig { config: updated } => {
            config.set_if_neq(Config(updated));
            return;
        }
        RendererRequest::AddAvatar { avatar } => {
            if windows.iter().any(|(_, window)| window.id == avatar.id) {
                output.error(Some(avatar.id), "Avatar ID already exists.");
            } else {
                commands.spawn(Window::new(avatar, config.0.fps, output.clone()));
            }
            return;
        }
        RendererRequest::RemoveAvatar { avatar_id } => (avatar_id, None),
        RendererRequest::SetAvatarConfig { avatar_id, config } => {
            (avatar_id, Some(WindowRequest::Configure(config)))
        }
        RendererRequest::Recenter { avatar_id } => (avatar_id, Some(WindowRequest::Recenter)),
        RendererRequest::Initialize { .. } | RendererRequest::Shutdown => unreachable!(),
    };
    match windows.iter().find(|(_, window)| window.id == id) {
        Some((entity, window)) => match request {
            Some(request) => window.command(request),
            None => {
                commands.entity(entity).despawn();
            }
        },
        None => output.error(Some(id), "Unknown avatar ID."),
    }
}

fn apply_config(config: Option<Res<Config>>, windows: Query<Ref<Window>>, output: Res<Output>) {
    if let Some(config) = config
        && config.is_changed()
    {
        for window in windows.iter().filter(|window| !window.is_added()) {
            window.command(WindowRequest::RenderConfig(config.0.clone()));
        }
        output
            .states
            .send_modify(|state| state.config = Some(config.0.clone()));
    }
}
