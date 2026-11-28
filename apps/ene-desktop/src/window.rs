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
    history: HashMap<String, BTreeMap<u64, w::Message>>,
    working: Option<String>,
    partial: String,
    connected: bool,
    notice: String,
    input: Entity<InputState>,
    _input_subscription: Subscription,
    settings: Settings,
    onboarding: crate::onboarding::Onboarding,
    settings_open: bool,
    config: PathBuf,
    local: Local,
    renderer: Option<Renderer>,
    playback: Playback,
    capture: Option<Capture>,
}

pub(crate) struct WindowInit {
    pub actions: mpsc::Sender<Action>,
    pub updates: mpsc::Receiver<Update>,
    pub renderer: Option<Renderer>,
    pub config: PathBuf,
    pub local: Local,
}

impl Window {
    pub(crate) fn new(window: &mut GpuiWindow, cx: &mut Context<Self>, init: WindowInit) -> Self {
        let WindowInit {
            actions,
            mut updates,
            renderer,
            config,
            local,
        } = init;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("メッセージを入力"));
        let input_subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.send_text(window, cx);
                }
            });
        let settings = Settings::new(window, cx);
        let onboarding = crate::onboarding::Onboarding::new(window, cx);
        cx.spawn(async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this
                    .update(cx, |window, cx| window.apply(update, cx))
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
                if this.update(cx, |window, _| window.poll_sound()).is_err() {
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
            settings,
            onboarding,
            settings_open: false,
            config,
            local,
            renderer,
            playback: Playback::new(),
            capture: None,
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
            Action::Chat | Action::Settings | Action::Select(_) => {
                if let Action::Select(ref companion) = action
                    && self
                        .snapshot
                        .companions
                        .iter()
                        .any(|item| item.id == *companion)
                {
                    self.companion.clone_from(companion);
                    self.submit(crate::network::Action::History(companion.clone()));
                }
                self.settings_open = matches!(action, Action::Settings);
                if self.settings_open {
                    self.submit(crate::network::Action::Settings);
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

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match update {
            Update::Connection(value) => {
                self.connected = value;
                if value {
                    self.submit(Action::Settings);
                } else {
                    self.onboarding.idle();
                    self.playback.clear();
                    self.working = None;
                    self.partial.clear();
                }
            }
            Update::Event(event) => {
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
                            self.submit(Action::History(self.companion.clone()));
                        }
                    }
                    Some(Payload::Message(message)) => {
                        self.history
                            .entry(message.companion_id.clone())
                            .or_default()
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
                        self.snapshot.activation = Some(*active).into();
                        if self.snapshot.activation.device_id != self.local.id {
                            self.playback.clear();
                        }
                    }
                    _ => {}
                }
            }
            Update::History(id, messages) => {
                let history = self.history.entry(id).or_default();
                for message in messages {
                    history.entry(message.position).or_insert(message);
                }
            }
            Update::Settings(settings) => self.settings.data = settings,
            Update::ClientPlugins(paths) => {
                self.local.plugins = paths;
                self.persist();
            }
            Update::Models(models) => self.settings.models = models,
            Update::Audit(audit) => self.settings.audit = audit,
            Update::Audio(audio) => {
                if self.local.speaker && self.playback.push(audio).is_err() {
                    self.notice = "音声出力を開けません。".into();
                }
                return;
            }
            Update::Notice(text) => self.notice = text,
            Update::SetupFinished => self.onboarding.idle(),
            Update::SetupFailed(text) => {
                self.onboarding.idle();
                self.notice = text;
            }
        }
        cx.notify();
    }

    fn send_text(&mut self, window: &mut GpuiWindow, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() || self.companion.is_empty() || !self.connected {
            return;
        }
        let send = Action::Send(w::SendRequest {
            mutation: mutation(0),
            companion_id: self.companion.clone(),
            text,
            ..Default::default()
        });
        if self.submit(send) {
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
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

    fn poll_sound(&mut self) {
        if let Some(renderer) = &self.renderer {
            renderer.lip(self.playback.companion.clone(), self.playback.rms());
        }
        if let Some(capture) = &self.capture {
            for segment in capture.take() {
                match segment {
                    Segment::Start => {
                        self.playback.clear();
                        self.submit(Action::Interrupt);
                    }
                    Segment::Audio { pcm, rate } => {
                        self.submit(Action::Capture(pcm, rate));
                    }
                }
            }
        }
    }

    fn persist(&self) {
        let tmp = self.config.with_extension("tmp");
        if let Ok(bytes) = serde_json::to_vec(&self.local)
            && std::fs::write(&tmp, bytes).is_ok()
        {
            let _failed = std::fs::rename(tmp, &self.config);
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

    fn render_chat(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let transcript = self.history.get(&self.companion);
        let messages = transcript
            .into_iter()
            .flat_map(|history| history.values())
            .filter(|message| message.role != w::message::Role::Trigger)
            .rev()
            .take(80)
            .map(|message| {
                let own = message.role == w::message::Role::User;
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .when(!own, |row| row.justify_start())
                    .child(
                        div()
                            .max_w(px(396.0))
                            .rounded_xl()
                            .px_4()
                            .py_2()
                            .bg(rgb(if own { 0xfce2ec } else { MILK }))
                            .border_1()
                            .border_color(rgb(PETAL))
                            .child(message.text.clone()),
                    )
            });
        let cards = self
            .snapshot
            .approvals
            .iter()
            .filter(|card| card.companion_id == self.companion)
            .cloned()
            .map(|card| {
                let actions = [
                    (w::approval_answer::Choice::Allow, "承認"),
                    (w::approval_answer::Choice::AlwaysAllow, "常に承認"),
                    (w::approval_answer::Choice::Deny, "拒否"),
                ]
                .into_iter()
                .filter(|(choice, _)| {
                    !card.card_only || *choice != w::approval_answer::Choice::AlwaysAllow
                })
                .map(|(choice, label)| {
                    let id = card.id.clone();
                    let digest = card.digest.clone();
                    let local_screen =
                        card.operation == "desktop.input" && card.device_id == self.local.id;
                    Button::new(format!("approval-{}-{label}", card.id))
                        .label(label)
                        .on_click(cx.listener(move |this, _, window, _| {
                            if local_screen
                                && !crate::computer::portal_session()
                                && choice != w::approval_answer::Choice::Deny
                            {
                                window.minimize_window();
                            }
                            this.submit(Action::Answer(w::ApprovalAnswer {
                                mutation: mutation(0),
                                id: id.clone(),
                                digest: digest.clone(),
                                choice: choice.into(),
                                ..Default::default()
                            }));
                        }))
                });
                div()
                    .rounded_lg()
                    .bg(rgb(0xfff3df))
                    .border_1()
                    .border_color(rgb(0xdfb67a))
                    .p_3()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(format!("操作の確認 · {}", card.operation)),
                    )
                    .child(div().text_sm().child(card.receiver))
                    .when(
                        card.operation == "desktop.input" && card.device_id == self.local.id,
                        |view| {
                            view.child(div().text_sm().child(
                                if crate::computer::portal_session() {
                                    "承認後、共有ポータルで送信する画面を選びます。"
                                } else {
                                    "承認すると Ene を最小化し、操作対象に戻ります。"
                                },
                            ))
                        },
                    )
                    .child(
                        div()
                            .text_sm()
                            .id(format!("approval-args-{}", card.id))
                            .max_h(px(112.0))
                            .overflow_y_scroll()
                            .child(card.arguments_json),
                    )
                    .child(div().flex().gap_2().mt_2().children(actions))
            });
        let running = self
            .snapshot
            .work
            .iter()
            .filter(|work| work.companion_id == self.companion)
            .filter(|work| {
                work.runs
                    .iter()
                    .any(|run| run.outcome == w::Outcome::Running)
            })
            .map(|work| work.id.clone())
            .collect::<Vec<_>>();
        let auto = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.automatic.load(Ordering::Relaxed));
        let holding = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.held.load(Ordering::Relaxed));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_3()
            .child(
                div()
                    .id("history")
                    .flex()
                    .flex_col_reverse()
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_3()
                    .when(!self.partial.is_empty(), |history| {
                        history.child(div().p_3().child(self.partial.clone()))
                    })
                    .children(messages),
            )
            .children(cards)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(
                        Button::new("send").label("送信").primary().on_click(
                            cx.listener(|this, _, window, cx| this.send_text(window, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("hold-mic")
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(rgb(if holding { PETAL } else { MILK }))
                            .cursor_pointer()
                            .child("押して話す")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.holding(true, cx)),
                            )
                            .on_mouse_up(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.holding(false, cx)),
                            )
                            .on_mouse_up_out(
                                MouseButton::Left,
                                cx.listener(|this, _, _, cx| this.holding(false, cx)),
                            ),
                    )
                    .child(
                        Button::new("auto")
                            .label(if auto {
                                "自動マイク オン"
                            } else {
                                "自動マイク オフ"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(capture) = this.microphone() {
                                    capture.automatic.store(!auto, Ordering::Relaxed);
                                }
                                if auto {
                                    this.capture.take();
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("speaker")
                            .label(if self.local.speaker {
                                "音声 オン"
                            } else {
                                "音声 オフ"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_speaker();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("stop")
                            .label("発話を停止")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.playback.clear();
                                this.submit(Action::Stop(w::StopRequest {
                                    mutation: mutation(0),
                                    id: this.companion.clone(),
                                    ..Default::default()
                                }));
                                cx.notify();
                            })),
                    ),
            )
            .when(!running.is_empty(), |view| {
                view.child(
                    Button::new("work-emergency-stop")
                        .label("進行中の作業を停止")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            for id in &running {
                                this.submit(Action::StopWork(w::StopRequest {
                                    mutation: mutation(0),
                                    id: id.clone(),
                                    ..Default::default()
                                }));
                            }
                            cx.notify();
                        })),
                )
            })
    }
}

impl Render for Window {
    fn render(&mut self, _: &mut GpuiWindow, cx: &mut Context<Self>) -> impl IntoElement {
        let clients = self.snapshot.companions.iter().cloned().map(|companion| {
            let active = companion.id == self.companion;
            Button::new(companion.id.clone())
                .label(companion.name)
                .when(active, Button::primary)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.companion.clone_from(&companion.id);
                    this.partial.clear();
                    this.working = None;
                    this.submit(Action::History(companion.id.clone()));
                    cx.notify();
                }))
        });
        let status = if self.connected {
            if self.working.is_some() {
                "応答中"
            } else {
                "接続済み"
            }
        } else {
            "ホストに接続しています…"
        };
        let top = div()
            .px_4()
            .py_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .bg(rgb(0xfadce6))
            .border_b_1()
            .border_color(rgb(PETAL))
            .child(div().flex().items_center().gap_2().children(clients))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_xs().child(status))
                    .child(
                        Button::new("settings")
                            .label(if self.settings_open {
                                "対話"
                            } else {
                                "設定"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings_open = !this.settings_open;
                                if this.settings_open {
                                    this.submit(Action::Settings);
                                }
                                cx.notify();
                            })),
                    ),
            );
        let main = if self.settings_open {
            self.settings
                .render(cx, &self.snapshot, &self.companion, &self.local)
                .into_any_element()
        } else if let Some(companion) = self
            .snapshot
            .companions
            .iter()
            .find(|item| {
                item.id == self.companion
                    && !item
                        .models
                        .iter()
                        .any(|model| model.role == w::model_selection::Role::Dialogue)
            })
            .filter(|_| self.connected)
        {
            self.onboarding
                .render(&self.settings.data, &self.settings.models, companion, cx)
                .into_any_element()
        } else {
            self.render_chat(cx).into_any_element()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BLUSH))
            .text_color(rgb(INK))
            .child(top)
            .when(
                self.connected
                    && !self.snapshot.activation.device_id.is_empty()
                    && self.local.id != self.snapshot.activation.device_id,
                |view| {
                    view.child(
                        div().px_4().py_2().child(
                            Button::new("active")
                                .label("この端末をアクティブにする")
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.submit(Action::Activate);
                                })),
                        ),
                    )
                },
            )
            .when(!self.notice.is_empty(), |view| {
                view.child(
                    div()
                        .px_4()
                        .py_2()
                        .text_sm()
                        .bg(rgb(0xfff3df))
                        .child(self.notice.clone()),
                )
            })
            .child(main)
    }
}

impl Window {
    pub(crate) fn start_setup(&mut self, action: Action, cx: &mut Context<Self>) {
        if self.submit(action) {
            self.onboarding.started();
            self.notice.clear();
        }
        cx.notify();
    }

    pub(crate) fn apply_settings(&mut self, action: Action, cx: &mut Context<Self>) {
        match &action {
            Action::Eyes(value) => {
                self.local.eyes = *value;
                self.persist();
            }
            Action::Input(value) => {
                self.local.input = *value;
                self.persist();
            }
            _ => {}
        }
        self.submit(action);
        cx.notify();
    }

    pub(crate) fn editor(&mut self) -> &mut Settings {
        &mut self.settings
    }
}

pub(crate) fn value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}
