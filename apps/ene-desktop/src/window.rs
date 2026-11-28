mod preferences;
mod render;

use crate::{
    Local,
    network::{Action, Update, mutation},
    renderer::Renderer,
    settings::Settings,
    sound::{Capture, Playback, Segment},
};
use ene_rpc::{self as w, event::Payload};
use gpui::component::{
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window as GpuiWindow, div, px, rgb,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::atomic::Ordering,
    time::Duration,
};
use tokio::sync::mpsc;

pub(crate) const INK: u32 = 0x3b232c;
pub(crate) const MILK: u32 = 0xfffdfd;
pub(crate) const PETAL: u32 = 0xf3c4d5;
pub(crate) const BLUSH: u32 = 0xfff4f8;

pub(crate) struct Window {
    actions: mpsc::Sender<Action>,
    snapshot: w::Snapshot,
    cursor: Option<w::Cursor>,
    companion: String,
    history: HashMap<String, Timeline>,
    working: Option<String>,
    partial: String,
    connected: bool,
    notice: String,
    input: Entity<InputState>,
    _input_subscription: Subscription,
    draft: Option<Draft>,
    settings: Settings,
    onboarding: crate::onboarding::Onboarding,
    settings_window: Option<gpui::AnyWindowHandle>,
    compact: bool,
    managed_host: bool,
    config: PathBuf,
    local: Local,
    renderer: Option<Renderer>,
    playback: Playback,
    capture: Option<Capture>,
    capture_generation: Option<u64>,
}

pub(crate) struct WindowInit {
    pub actions: mpsc::Sender<Action>,
    pub updates: mpsc::Receiver<Update>,
    pub renderer: Option<Renderer>,
    pub config: PathBuf,
    pub local: Local,
    pub managed_host: bool,
}

#[derive(Default)]
struct Timeline {
    messages: BTreeMap<u64, w::Message>,
    next_before: Option<u64>,
}

struct Draft {
    request: w::SendRequest,
    sending: bool,
}

impl Window {
    pub(crate) fn new(window: &mut GpuiWindow, cx: &mut Context<Self>, init: WindowInit) -> Self {
        let WindowInit {
            actions,
            mut updates,
            renderer,
            config,
            local,
            managed_host,
        } = init;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("メッセージを入力"));
        let input_subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.send_text(cx);
                }
            });
        let settings = Settings::new(window, cx);
        let onboarding = crate::onboarding::Onboarding::new(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.apply(update, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let timer = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                timer.timer(Duration::from_millis(16)).await;
                if this.update(cx, |window, cx| window.poll_sound(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        Self {
            actions,
            snapshot: w::Snapshot::default(),
            cursor: None,
            companion: String::new(),
            history: HashMap::new(),
            working: None,
            partial: String::new(),
            connected: false,
            notice: String::new(),
            input,
            _input_subscription: input_subscription,
            draft: None,
            settings,
            onboarding,
            settings_window: None,
            compact: false,
            managed_host,
            config,
            local,
            renderer,
            playback: Playback::new(),
            capture: None,
            capture_generation: None,
        }
    }

    pub(crate) fn tray_action(
        &mut self,
        action: crate::tray::Action,
        window: &mut GpuiWindow,
        cx: &mut Context<Self>,
    ) {
        use crate::tray::Action;
        match action {
            Action::Settings => self.open_settings(cx),
            Action::Chat | Action::Select(_) => {
                if let Action::Select(ref companion) = action
                    && self
                        .snapshot
                        .companions
                        .iter()
                        .any(|item| item.id == *companion)
                {
                    self.companion.clone_from(companion);
                    self.submit(crate::network::Action::History(companion.clone(), 0));
                }
                cx.activate(true);
                window.activate_window();
            }
            Action::Hide => window.minimize_window(),
            Action::Mute => self.toggle_speaker(),
            Action::Center => {
                if let Some(renderer) = self.renderer.clone() {
                    let character = self.companion.clone();
                    cx.spawn(async move |_, _| {
                        let _result = renderer
                            .command(ene_core::avatar::Command::Center {
                                character,
                                monitor: None,
                            })
                            .await;
                    })
                    .detach();
                }
            }
            Action::Quit => cx.quit(),
        }
        cx.notify();
    }

    fn toggle_speaker(&mut self) {
        self.local.speaker = !self.local.speaker;
        if !self.local.speaker {
            self.playback.clear();
        }
        self.submit(Action::Speaker(self.local.speaker));
        self.persist();
    }

    fn apply(&mut self, update: Update, window: &mut GpuiWindow, cx: &mut Context<Self>) {
        match update {
            Update::Connection(value) => {
                self.connected = value;
                if value {
                    self.submit(Action::Settings);
                } else {
                    self.onboarding.idle();
                    if let Some(draft) = &mut self.draft {
                        draft.sending = false;
                    }
                    self.playback.clear();
                    self.capture_generation = None;
                    self.working = None;
                    self.partial.clear();
                }
            }
            Update::Event(event) => self.apply_event(event),
            Update::History(id, before, page) => {
                let timeline = self.history.entry(id).or_default();
                if timeline.next_before.is_none() || timeline.next_before == Some(before) {
                    timeline.next_before = Some(page.next_before);
                }
                for message in page.messages {
                    timeline.messages.entry(message.position).or_insert(message);
                }
            }
            Update::Settings(settings) => self.settings.data = settings,
            Update::ClientPlugins(paths) => {
                self.local.plugins = paths;
                self.persist();
            }
            Update::Workspace(path) => {
                self.local.workspace = Some(path);
                self.persist();
                self.notice = "作業フォルダーを変更しました。".into();
            }
            Update::AvatarScale(scale) => {
                self.local.avatar_scale = scale;
                self.persist();
            }
            Update::Transcribed(text) => self.settings.voice.transcript = text,
            Update::Models(models) => self.settings.models = models,
            Update::McpTools(tools) => {
                self.notice = format!("公開可能な MCP ツール · {}", tools.join(" / "))
            }
            Update::Audit(audit) => self.settings.audit = audit,
            Update::Audio(audio) => {
                if self.local.speaker && self.playback.push(audio).is_err() {
                    self.notice = "音声出力を開けません。".into();
                }
                return;
            }
            Update::Notice(text) => self.notice = text,
            Update::SendResult { key, error } => {
                if let Some(draft) = &mut self.draft
                    && draft.request.mutation.request_key == key
                {
                    if let Some(error) = error {
                        draft.sending = false;
                        self.notice = error;
                    } else {
                        if self.input.read(cx).value().trim() == draft.request.text {
                            self.input
                                .update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        self.draft = None;
                    }
                }
            }
            Update::SetupFinished => self.onboarding.idle(),
            Update::SetupFailed(text) => {
                self.onboarding.idle();
                self.notice = text;
            }
        }
        cx.notify();
    }

    fn apply_event(&mut self, event: w::Event) {
        let cursor = event.cursor.into_option().unwrap_or_default();
        if self
            .cursor
            .as_ref()
            .is_some_and(|old| old.epoch == cursor.epoch && old.sequence >= cursor.sequence)
            && !matches!(event.payload, Some(Payload::Snapshot(_)))
        {
            return;
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|old| old.epoch != cursor.epoch)
        {
            self.history.clear();
        }
        self.cursor = Some(cursor);
        match event.payload {
            Some(Payload::Snapshot(snapshot)) => {
                self.snapshot = *snapshot;
                if !self
                    .snapshot
                    .companions
                    .iter()
                    .any(|item| item.id == self.companion)
                {
                    self.companion = self
                        .snapshot
                        .companions
                        .first()
                        .map_or_else(String::new, |item| item.id.clone());
                }
                if !self.companion.is_empty() {
                    self.submit(Action::History(self.companion.clone(), 0));
                }
            }
            Some(Payload::Message(message)) => {
                self.history
                    .entry(message.companion_id.clone())
                    .or_default()
                    .messages
                    .insert(message.position, *message);
            }
            Some(Payload::Delta(delta))
                if delta.companion_id == self.companion
                    && self.working.as_deref() == Some(&delta.turn_id) =>
            {
                if delta.reset {
                    self.partial.clear();
                } else {
                    self.partial.push_str(&delta.text);
                }
            }
            Some(Payload::Turn(turn)) if turn.companion_id == self.companion => {
                if turn.outcome == w::Outcome::Running {
                    self.working = Some(turn.turn_id.clone());
                    self.partial.clear();
                } else if self.working.as_deref() == Some(&turn.turn_id) {
                    self.working = None;
                    self.partial.clear();
                }
            }
            Some(Payload::Work(work)) => {
                self.snapshot.work.retain(|previous| previous.id != work.id);
                self.snapshot.work.push(*work);
            }
            Some(Payload::Approval(approval)) => self.snapshot.approvals.push(*approval),
            Some(Payload::Notice(note))
                if note.companion_id.is_empty() || note.companion_id == self.companion =>
            {
                self.notice = note.text;
            }
            Some(Payload::Activation(active)) => {
                if active.companion_id == self.playback.companion
                    && active.device_id != self.local.id
                {
                    self.playback.clear();
                }
                self.snapshot
                    .activations
                    .retain(|previous| previous.companion_id != active.companion_id);
                self.snapshot.activations.push(*active);
            }
            _ => {}
        }
    }

    fn send_text(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty()
            || self.companion.is_empty()
            || !self.connected
            || self.draft.as_ref().is_some_and(|draft| draft.sending)
        {
            return;
        }
        let request = self
            .draft
            .as_ref()
            .filter(|draft| {
                draft.request.text == text && draft.request.companion_id == self.companion
            })
            .map_or_else(
                || w::SendRequest {
                    mutation: mutation(0),
                    companion_id: self.companion.clone(),
                    text: text.clone(),
                    ..Default::default()
                },
                |draft| draft.request.clone(),
            );
        if self.submit(Action::Send(request.clone())) {
            self.draft = Some(Draft {
                request,
                sending: true,
            });
            self.notice.clear();
        }
        cx.notify();
    }

    fn submit(&mut self, action: Action) -> bool {
        if self.actions.try_send(action).is_err() {
            self.notice = "処理中です。もう一度お試しください。".into();
            return false;
        }
        true
    }

    fn poll_sound(&mut self, cx: &mut Context<Self>) {
        if let Some(renderer) = &self.renderer {
            renderer.lip(self.playback.companion.clone(), self.playback.rms());
        }
        if let Some(capture) = &self.capture {
            let level = capture.level();
            if self.settings.voice.recording && self.settings.voice.level != level {
                self.settings.voice.level = level;
                cx.notify();
            }
            for segment in capture.take() {
                match segment {
                    Segment::Start { diagnostic } => {
                        self.capture_generation = self
                            .snapshot
                            .activations
                            .iter()
                            .find(|active| {
                                active.companion_id == self.companion
                                    && active.device_id == self.local.id
                            })
                            .map(|active| active.generation);
                        if !diagnostic && let Some(generation) = self.capture_generation {
                            self.playback.clear();
                            self.submit(Action::Interrupt(generation));
                        }
                    }
                    Segment::Audio {
                        pcm,
                        rate,
                        diagnostic,
                    } => {
                        if let Some(generation) = self.capture_generation.take() {
                            self.submit(Action::Capture(generation, pcm, rate, diagnostic));
                        }
                    }
                }
            }
        }
    }

    fn persist(&self) {
        if self.local.save(&self.config).is_err() {
            tracing::warn!("desktop settings could not be saved");
        }
    }

    fn microphone(&mut self) -> Option<&Capture> {
        if self.capture.is_none() {
            match Capture::open() {
                Ok(capture) => self.capture = Some(capture),
                Err(_) => self.notice = "マイクを開けません。".into(),
            }
        }
        self.capture.as_ref()
    }

    fn holding(&mut self, value: bool, cx: &mut Context<Self>) {
        if value && !self.connected {
            return;
        }
        if value {
            self.microphone();
        }
        if let Some(capture) = &self.capture {
            capture.held.store(value, Ordering::Relaxed);
        }
        cx.notify();
    }
}

pub(crate) fn value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}
