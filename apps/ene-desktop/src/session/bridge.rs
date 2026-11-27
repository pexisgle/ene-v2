//! Avatar renderer, OS perception sensor, computer use, workspace, and plugin bridge.

use super::transport::SessionTransport;
use super::{BACKGROUND_DRAIN_TIMEOUT, MAX_RETRY, RECONNECT_BACKOFF, SAMPLE, SCREEN_SAMPLE};
use crate::{
    avatar::{self, Avatar},
    host::Host,
    perception::{self, NotifyOn, Report, Sensor},
    state::Device,
    update::Update,
    work_tools::Workspace,
};
use backon::{BackoffBuilder as _, ExponentialBackoff};
use ene_avatar_ipc::{AvatarEvent, AvatarStatus};
use ene_rpc as w;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc::UnboundedSender;

pub(super) struct DeviceBridge {
    pub(super) avatar: Option<Avatar>,
    renderer_retry: RendererRetry,
    /// Current avatar display scale percentage.
    pub(super) avatar_scale: f32,
    pub(super) sensor: Sensor,
    pub(super) eyes_enabled: bool,
    #[cfg(windows)]
    pub(super) computer_enabled: bool,
    #[cfg(windows)]
    pub(super) computer_frame: Option<crate::computer::Frame>,
    pub(super) workspace: Option<Arc<Workspace>>,
    /// Verified client-side Wasm plugins (`F11`).
    pub(super) client_plugins: Arc<crate::plugins::ClientPlugins>,
    /// Fire-and-forget work whose completion nobody waits on but which must not
    /// outlive the session: draining it on shutdown is the whole point. The
    /// mutex is held only for the spawn call, never across an await.
    pub(super) background: parking_lot::Mutex<tokio::task::JoinSet<()>>,
    /// Per-character debounce timestamp for avatar click/interaction perception (`F21`).
    pub(super) last_avatar_interaction: HashMap<String, std::time::Instant>,
}

struct RendererRetry {
    not_before: tokio::time::Instant,
    delay: ExponentialBackoff,
}

impl RendererRetry {
    fn new() -> Self {
        Self {
            not_before: tokio::time::Instant::now(),
            delay: RECONNECT_BACKOFF.build(),
        }
    }

    fn failed(&mut self, uptime: Duration) {
        if uptime >= Duration::from_secs(30) {
            self.delay = RECONNECT_BACKOFF.build();
        }
        self.not_before = tokio::time::Instant::now() + self.delay.next().unwrap_or(MAX_RETRY);
    }
}

impl DeviceBridge {
    pub(super) async fn new() -> Self {
        let workspace = Workspace::probe().map(Arc::new);
        let client_plugins = Arc::new(crate::plugins::ClientPlugins::probe_default().await);
        Self {
            avatar: None,
            renderer_retry: RendererRetry::new(),
            avatar_scale: 100.,
            sensor: Sensor::default(),
            eyes_enabled: perception::enabled(),
            #[cfg(windows)]
            computer_enabled: perception::preference_enabled(crate::computer::PREFERENCE),
            #[cfg(windows)]
            computer_frame: None,
            workspace,
            client_plugins,
            background: parking_lot::Mutex::new(tokio::task::JoinSet::new()),
            last_avatar_interaction: HashMap::new(),
        }
    }

    /// Opens the avatar renderer, returning `Ok(true)` if spawned and synced,
    /// `Ok(false)` when no renderer is configured, or an unavailable status on error.
    pub(super) async fn open_avatar(
        &mut self,
        transport: &SessionTransport,
    ) -> Result<bool, Device> {
        if self.avatar.is_some()
            || transport.bindings.is_empty()
            || tokio::time::Instant::now() < self.renderer_retry.not_before
        {
            return Ok(false);
        }
        let Some(program) = transport.options.renderer.as_deref() else {
            return Ok(false);
        };
        match Avatar::spawn(program, &transport.options.id) {
            Ok(avatar) => {
                self.avatar = Some(avatar);
                self.sync_avatars(transport).await;
                Ok(true)
            }
            Err(error) => {
                self.renderer_retry.failed(Duration::ZERO);
                tracing::warn!("the renderer did not start: {error}");
                Err(Device::Unavailable(error.to_string()))
            }
        }
    }

    /// Reconciles all resident companions onto the running renderer with their
    /// resolved `BodyBinding` models and placements.
    pub(super) async fn sync_avatars(&mut self, transport: &SessionTransport) {
        let Some(avatar) = self.avatar.as_mut() else {
            return;
        };
        avatar.retain_characters(&transport.bindings).await;
        let idle = avatar::motion("idle");
        for id in &transport.bindings {
            let body = transport.body_binding_for(id);
            if let Some(model) = transport.resolve_model(&body)
                && let Err(error) = avatar
                    .show_with_body(id, &model, idle.as_deref(), &body, self.avatar_scale)
                    .await
            {
                tracing::warn!(character = %id, %error, "the renderer did not show the model");
            }
        }
    }

    /// Returns to idle when the renderer finishes a gesture and detects dead renderers.
    pub(super) async fn drain_renderer(&mut self, transport: &SessionTransport) -> bool {
        let Some(mut avatar) = self.avatar.take() else {
            return false;
        };
        let mut changed = false;
        for event in avatar.notifications() {
            tracing::debug!(?event, "renderer");
            changed |= matches!(event, AvatarEvent::State { .. });
            self.handle_renderer_event(transport, &mut avatar, event)
                .await;
        }
        if avatar.is_healthy() {
            self.avatar = Some(avatar);
            changed
        } else {
            self.renderer_retry.failed(avatar.uptime());
            avatar.shutdown().await;
            true
        }
    }

    async fn handle_renderer_event(
        &mut self,
        transport: &SessionTransport,
        avatar: &mut Avatar,
        event: AvatarEvent,
    ) {
        match event {
            AvatarEvent::Finished {
                character,
                request_id,
            } => {
                if transport.bindings.contains(&character)
                    && let Err(error) = avatar.return_to_idle(&character, request_id).await
                {
                    tracing::warn!(%character, %error, "the renderer could not return to idle");
                }
            }
            AvatarEvent::State {
                character,
                status: AvatarStatus::Error,
                error,
            } => {
                tracing::warn!(%character, ?error, "the renderer reported a model error");
            }
            AvatarEvent::Click { character } => self.emit_avatar_click(transport, character),
            AvatarEvent::State { .. } => {}
        }
    }

    fn emit_avatar_click(&mut self, transport: &SessionTransport, character: String) {
        const DEBOUNCE: Duration = Duration::from_millis(500);
        let now = std::time::Instant::now();
        let allow = self
            .last_avatar_interaction
            .get(&character)
            .is_none_or(|prev| now.duration_since(*prev) >= DEBOUNCE);
        if !allow || !transport.bindings.contains(&character) {
            return;
        }
        self.last_avatar_interaction.insert(character.clone(), now);
        let Some(host) = transport.current_host() else {
            return;
        };
        let event = w::PerceptionEvent {
            client_id: transport.options.id.clone(),
            companion_id: Some(character.clone()),
            event: Some(w::perception_event::Event::AvatarInteraction(Box::new(
                w::AvatarInteraction {
                    kind: w::AvatarInteractionKind::Click.into(),
                    character_id: character,
                    region: "body".into(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        // Tracked so shutdown can wait for the last report to land.
        self.background.lock().spawn(async move {
            let _sampled = host.client.report_perception(event).await;
        });
    }

    pub(super) fn set_eyes_enabled(&mut self, enabled: bool, updates: &UnboundedSender<Update>) {
        match perception::save_enabled(enabled) {
            Ok(()) => self.eyes_enabled = enabled,
            Err(error) => {
                if !enabled {
                    self.eyes_enabled = false;
                }
                super::notify_ui(
                    updates,
                    Update::Failed {
                        context: "「目」の設定を保存",
                        message: error.to_string(),
                    },
                );
            }
        }
        if !self.eyes_enabled {
            self.sensor = Sensor::default();
        }
        super::notify_ui(updates, Update::EyesEnabled(self.eyes_enabled));
    }

    #[cfg(windows)]
    pub(super) fn set_computer_use_enabled(
        &mut self,
        enabled: bool,
        updates: &UnboundedSender<Update>,
    ) {
        self.computer_frame = None;
        if !enabled {
            self.computer_enabled = false;
        }
        match perception::save_preference(crate::computer::PREFERENCE, enabled) {
            Ok(()) => self.computer_enabled = enabled,
            Err(error) => {
                super::notify_ui(
                    updates,
                    Update::Failed {
                        context: "画面操作の設定を保存",
                        message: error.to_string(),
                    },
                );
            }
        }
        super::notify_ui(updates, Update::ComputerUseEnabled(self.computer_enabled));
    }

    /// Keeps socket waits and screenshot encoding off the audio/device loop.
    pub(super) fn sample(
        &mut self,
        transport: &SessionTransport,
        host: &Host,
        screen_at: &mut tokio::time::Instant,
        eyes_open: &mut bool,
        observations: &mut tokio::task::JoinSet<()>,
    ) {
        use w::perception_event::Event;
        if !self.eyes_enabled
            || transport.bindings.is_empty()
            || transport.subscription.is_empty()
            || !perception::supported()
        {
            return;
        }
        #[cfg(windows)]
        if !perception::unlocked() {
            if std::mem::take(eyes_open) {
                observations.abort_all();
                self.sensor.reset();
                let event = transport.perception_event(Event::Enabled(false));
                let host = host.clone();
                observations.spawn(async move {
                    let _discarded =
                        tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
                });
            }
            return;
        }
        let now = tokio::time::Instant::now();
        let periodic = !*eyes_open || now >= *screen_at;
        let mut events = Vec::new();
        if periodic {
            *screen_at = now + SCREEN_SAMPLE;
            *eyes_open = true;
            events.push(Event::Enabled(true));
        }
        events.extend(
            self.sensor
                .observe(perception::observe(), periodic)
                .into_iter()
                .map(perception_report_event),
        );
        if events.is_empty() {
            return;
        }
        let events = events
            .into_iter()
            .map(|event| transport.perception_event(event))
            .collect::<Vec<_>>();
        let host = host.clone();
        #[cfg(windows)]
        let screen =
            periodic.then(|| transport.perception_event(Event::ScreenFrame(Box::default())));
        observations.spawn(async move {
            for event in events {
                let result =
                    tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
                if !matches!(result, Ok(Ok(_))) {
                    tracing::debug!("perception was not reported");
                    return;
                }
            }
            #[cfg(windows)]
            if let Some(mut event) = screen
                && let Ok(Some(jpeg)) = tokio::task::spawn_blocking(perception::capture).await
            {
                event.event = Some(Event::ScreenFrame(Box::new(w::ScreenFrame {
                    jpeg,
                    ..Default::default()
                })));
                let _sampled =
                    tokio::time::timeout(SAMPLE, host.client.report_perception(event)).await;
            }
        });
    }

    pub(super) fn body_available(&self, transport: &SessionTransport) -> bool {
        self.avatar
            .as_ref()
            .is_some_and(|avatar| transport.bindings.iter().any(|id| avatar.is_ready(id)))
    }

    pub(super) fn avatar_state(&self, transport: &SessionTransport) -> Device {
        if transport.bindings.is_empty() && transport.options.renderer.is_some() {
            return Device::Off;
        }
        match self.avatar.as_ref() {
            Some(avatar) => {
                let labels: Vec<String> = transport
                    .bindings
                    .iter()
                    .filter_map(|id| avatar.describe(id))
                    .collect();
                if !labels.is_empty() {
                    return Device::Ready(labels.join(", "));
                }
                let reason = transport.bindings.iter().find_map(|id| avatar.failure(id));
                Device::Unavailable(reason.unwrap_or("モデルの描画を準備しています").into())
            }
            None if transport.options.renderer.is_some() => {
                Device::Unavailable("描画プロセスが動作していません".into())
            }
            None => Device::Unavailable("描画プロセスが設定されていません".into()),
        }
    }

    /// Stops the renderer and drains the fire-and-forget work.
    ///
    /// Dropping the child kills it, so that is the whole of the client's
    /// process ownership. The background tasks are the last perception and
    /// speech reports; aborting them would drop a report the host is waiting
    /// for, so they get a bounded chance to finish first.
    pub(super) async fn shutdown(&mut self) {
        if let Some(avatar) = self.avatar.take() {
            avatar.shutdown().await;
        }
        // The set is moved out so the mutex guard is not held across an await.
        let mut background = std::mem::take(&mut *self.background.lock());
        let drained = tokio::time::timeout(BACKGROUND_DRAIN_TIMEOUT, async {
            while background.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            tracing::debug!(
                remaining = background.len(),
                "background reports did not finish before shutdown"
            );
            background.abort_all();
        }
    }

    /// Reaps finished background tasks so the set does not grow for the life
    /// of the session.
    pub(super) fn reap_background(&mut self) {
        let mut background = self.background.lock();
        while let Some(result) = background.try_join_next() {
            if let Err(error) = result
                && error.is_panic()
            {
                tracing::warn!("a background report panicked: {error}");
            }
        }
    }
}

fn perception_report_event(report: Report) -> w::perception_event::Event {
    use w::perception_event::Event;
    match report {
        Report::Window(window) => Event::ActiveWindow(Box::new(
            window.map_or_else(w::ActiveWindowChanged::default, window_report),
        )),
        Report::Interrupt(window, transition) => Event::WatchMatch(Box::new(w::WatchMatch {
            window: Some(window_report(window)).into(),
            transition: match transition {
                NotifyOn::Focus => w::WatchTransition::Focus,
                NotifyOn::Blur => w::WatchTransition::Blur,
                NotifyOn::TitleChange => w::WatchTransition::TitleChange,
            }
            .into(),
            ..Default::default()
        })),
        Report::Activity(idle_seconds) => Event::UserActivity(Box::new(w::UserActivityState {
            is_active: idle_seconds.is_some_and(|seconds| seconds < perception::IDLE_THRESHOLD),
            idle_seconds: idle_seconds.unwrap_or_default(),
            unavailable: idle_seconds.is_none(),
            ..Default::default()
        })),
    }
}

fn window_report(window: perception::Window) -> w::ActiveWindowChanged {
    w::ActiveWindowChanged {
        title: window.title,
        app_id: window.app_id,
        ..Default::default()
    }
}
