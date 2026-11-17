//! The window: one entity owns what it draws, and every action is one request.
//!
//! The host is the authority. This entity holds the display copy of the host's
//! state, the selected companion's history and the operator's own input, and it
//! does nothing on a timer. The session pushes `Update`s into one channel, the
//! pump below folds them into the model, and each user action spawns a single
//! request whose result comes back the same way.
//!
//! Nothing here reimplements the host: no model choice, no fallback, no memory
//! inference, no orchestration. Settings edits carry the revision they were read
//! at, and a conflict refetches instead of overwriting.

mod chat;
mod companion;
mod settings;

use crate::{
    input::{self, Draft},
    session::{Connection, Options},
    state::{Link, Model, NoticeKind},
    update::{Command, Update},
};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, Entity, Render, SharedString, Task, Window, div,
    prelude::*, px, rgb, rgba,
};
use std::collections::BTreeMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

gpui::actions!(ene, [Submit, StopPlayback]);

/// Which surface the window is showing. The roster is an overlay, and approvals
/// are not a surface of their own: the design consolidates the approval card
/// into the companion's window and never shows the agents behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panel {
    Chat,
    Settings,
}

/// Which part of settings is open. Grouping matters more than labels: a user
/// should be able to tell where a setting lives without reading the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    Companion,
    New,
    Connections,
    Voice,
    Device,
}

impl SettingsSection {
    const ALL: [SettingsSection; 5] = [
        SettingsSection::Companion,
        SettingsSection::New,
        SettingsSection::Connections,
        SettingsSection::Voice,
        SettingsSection::Device,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsSection::Companion => "コンパニオン",
            SettingsSection::New => "新しいコンパニオン",
            SettingsSection::Connections => "サービス接続",
            SettingsSection::Voice => "音声読み上げ",
            SettingsSection::Device => "この端末",
        }
    }

    fn key(self) -> &'static str {
        match self {
            SettingsSection::Companion => "settings-companion",
            SettingsSection::New => "settings-new",
            SettingsSection::Connections => "settings-connections",
            SettingsSection::Voice => "settings-voice",
            SettingsSection::Device => "settings-device",
        }
    }
}

/// How a control looks. Colour carries meaning, not decoration.
#[derive(Clone, Copy)]
pub(crate) enum Tone {
    Primary,
    Plain,
    Danger,
}

pub struct Desktop {
    pub(crate) model: Model,
    pub(crate) options: Options,
    connection: Connection,
    commands: UnboundedSender<Command>,
    updates: UnboundedSender<Update>,
    runtime: tokio::runtime::Handle,
    pub(crate) panel: Panel,
    pub(crate) roster_open: bool,
    pub(crate) settings_section: SettingsSection,
    companion_details_open: bool,
    /// The model chosen for a companion that is being created.
    pub(crate) new_model: Option<(String, String)>,
    pub(crate) chat_input: Entity<Draft>,
    /// Fields created on demand, keyed by what they edit.
    drafts: BTreeMap<String, Entity<Draft>>,
    pub(crate) scroll: gpui::ScrollHandle,
    _pump: Task<()>,
}

impl Desktop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut Context<Self>,
        options: Options,
        connection: Connection,
        commands: UnboundedSender<Command>,
        updates_tx: UnboundedSender<Update>,
        mut updates: UnboundedReceiver<Update>,
        runtime: tokio::runtime::Handle,
        discovery: String,
    ) -> Self {
        let chat_input =
            cx.new(|cx| Draft::new(cx, "メッセージを入力（Enterで送信）", "ChatInput"));
        let pump = cx.spawn(async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this.update(cx, |this, cx| this.apply(update, cx)).is_err() {
                    // The window is gone; the session exits when its update
                    // channel closes.
                    break;
                }
            }
        });
        Self {
            model: Model::new(discovery),
            options,
            connection,
            commands,
            updates: updates_tx,
            runtime,
            panel: Panel::Chat,
            roster_open: false,
            settings_section: SettingsSection::Companion,
            companion_details_open: false,
            new_model: None,
            chat_input,
            drafts: BTreeMap::new(),
            scroll: gpui::ScrollHandle::new(),
            _pump: pump,
        }
    }

    /// Folds one update in and performs what it invalidated.
    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        // What the model does not own: the field that held a secret, and a fresh
        // snapshot the window asked for itself.
        match &update {
            Update::CredentialRegistered { connection_id } => {
                if let Some(draft) = self.drafts.get(&format!("secret:{connection_id}")).cloned() {
                    draft.update(cx, |draft, cx| draft.clear(cx));
                }
            }
            Update::Resnapshot => self.resnapshot(),
            Update::Created(id) => {
                // A fresh companion is the one the user wants to see.
                self.new_model = None;
                for key in ["new:name", "new:mind"] {
                    if let Some(draft) = self.drafts.get(key) {
                        draft.update(cx, |draft, cx| draft.clear(cx));
                    }
                }
                self.panel = Panel::Chat;
                self.resnapshot();
                self.select(id, cx);
            }
            _ => {}
        }
        let conflicted = matches!(update, Update::Conflict { .. });
        if let Update::SendFailed { reason, .. } = &update {
            // The window keeps the text as unsent, and the reason is worth a log
            // line: a failed send is otherwise invisible once the notice ages out.
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        // Settings decide whether this client has an audio output at all, and
        // that stream is opened once per connection. A connection the user just
        // added would otherwise be invisible until the next restart.
        let settings_changed = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(w::notification::Event::SettingsUpdated(_))
                )
        );
        let had_companion = self.model.companion_id().is_some();
        let old_name = self
            .model
            .companion()
            .map(|companion| companion.name.clone());
        let had_approvals = self.model.approvals().len();
        let effects = self.model.apply(update);
        // The link goes live before the snapshot names a companion, so the
        // first fetch can run with nothing selected and never repeat.
        let selected_now = !had_companion && self.model.companion_id().is_some();
        // The composer's hint follows the companion on screen, not only the
        // first selection, so a rename in settings is visible immediately.
        if old_name
            != self
                .model
                .companion()
                .map(|companion| companion.name.clone())
        {
            self.refresh_placeholder(cx);
        }
        if settings_changed {
            let _ = self.commands.send(Command::Refresh);
        }
        if conflicted {
            // A revision moved under the window. The host is the authority, so
            // refetch instead of retrying with a guess.
            self.resnapshot();
            self.open_panel(cx);
        }
        // A confirmation the user has to act on must not arrive below the fold.
        if self.model.approvals().len() > had_approvals {
            self.scroll.scroll_to_bottom();
        }
        if effects.history || selected_now {
            self.load_history(cx);
        }
        if effects.approvals || selected_now {
            self.load_approvals(cx);
        }
        cx.notify();
    }

    /// The generated clients, or nothing while the link is down.
    pub(crate) fn host(&self) -> Option<crate::host::Host> {
        self.connection.read().ok()?.clone()
    }

    /// Whether the window can send requests right now.
    pub(crate) fn live(&self) -> bool {
        self.model.link.is_live() && self.host().is_some()
    }

    /// Spawns one request, mapping a failure by its Connect code.
    pub(crate) fn request<T, D>(
        &self,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
    {
        self.request_with(context, future, done, failure)
    }

    /// Spawns one request whose failure the caller interprets.
    pub(crate) fn request_with<T, D, E>(
        &self,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
        error: E,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
        E: FnOnce(&'static str, connectrpc::ConnectError) -> Update + Send + 'static,
    {
        let updates = self.updates.clone();
        self.runtime.spawn(async move {
            let update = match future.await {
                Ok(value) => done(value),
                Err(failed) => error(context, failed),
            };
            let _ = updates.send(update);
        });
    }

    /// A text field, created the first time a panel needs it.
    pub(crate) fn draft(
        &mut self,
        cx: &mut Context<Self>,
        key: impl Into<String>,
        placeholder: &'static str,
        context: &'static str,
    ) -> Entity<Draft> {
        self.drafts
            .entry(key.into())
            .or_insert_with(|| cx.new(|cx| Draft::new(cx, placeholder, context)))
            .clone()
    }

    /// Refreshes the window from the host's whole state.
    pub(crate) fn resnapshot(&self) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "ホスト状態の取得",
            async move {
                let response = host.host.get_snapshot(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |snapshot| {
                Update::Notification(Box::new(w::Notification {
                    kind: "initial_state".into(),
                    event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
                    ..Default::default()
                }))
            },
        );
    }

    /// Sends a message, keeping the text and its key if the host refuses it.
    pub(crate) fn send(&mut self, text: String, cx: &mut Context<Self>) {
        // The key is what makes a retry idempotent: the host returns the
        // original acceptance for the same key and content.
        let request_key = uuid::Uuid::new_v4().to_string();
        self.send_keyed(text, request_key, cx);
    }

    /// Sends text under a key the caller already holds, so a retry is safe.
    pub(crate) fn send_keyed(&mut self, text: String, request_key: String, cx: &mut Context<Self>) {
        let Some(companion) = self.model.companion_id().map(str::to_owned) else {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "コンパニオンがいません",
            );
            cx.notify();
            return;
        };
        if !self.live() {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "ホストに接続していないため送信できません",
            );
            cx.notify();
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.model.chat.sending.push(crate::state::Sending {
            request_key: request_key.clone(),
            text: text.clone(),
        });
        tracing::info!(companion = %companion, key = %request_key, chars = text.chars().count(), "message sent");
        self.chat_input.update(cx, |draft, cx| draft.clear(cx));
        let updates = self.updates.clone();
        self.runtime.spawn(async move {
            let outcome = match host
                .chat
                .send_message(w::SendMessageRequest {
                    companion_id: companion,
                    request_key: request_key.clone(),
                    text: text.clone(),
                    client_id: String::new(),
                    ..Default::default()
                })
                .await
            {
                Ok(response) => response
                    .view()
                    .to_owned_message()
                    .map(|acceptance| acceptance.request_id)
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let update = match outcome {
                Ok(request_id) => Update::Accepted {
                    request_key,
                    request_id,
                },
                Err(reason) => Update::SendFailed {
                    request_key,
                    text,
                    reason,
                },
            };
            let _ = updates.send(update);
        });
        cx.notify();
    }

    /// Stops playback now and asks the host to stop the turn. The turn is only
    /// stopped when the host says so, which arrives as a `TurnEvent`.
    fn stop(&mut self, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::StopPlayback);
        let Some(companion) = self.model.companion_id().map(str::to_owned) else {
            cx.notify();
            return;
        };
        let Some(host) = self.host() else {
            cx.notify();
            return;
        };
        let stopping = companion.clone();
        self.request(
            "生成の停止",
            async move {
                host.chat
                    .stop(w::StopRequest {
                        companion_id: companion,
                        ..Default::default()
                    })
                    .await
            },
            move |_response| Update::StopRequested {
                companion_id: stopping,
            },
        );
        cx.notify();
    }
}

impl Render for Desktop {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("shell")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(0xfaf9f7))
            .text_color(rgb(0x1c1917))
            .font_family("Yu Gothic UI")
            .child(self.render_header(cx))
            .child(match self.panel {
                Panel::Chat => self.render_chat(cx),
                Panel::Settings => self.render_settings(cx),
            })
            .when(self.roster_open, |this| this.child(self.render_roster(cx)))
    }
}

impl Desktop {
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.panel == Panel::Settings {
            return div()
                .flex()
                .items_center()
                .gap_4()
                .h(px(56.))
                .flex_none()
                .px_4()
                .border_b_1()
                .border_color(line())
                .bg(card())
                .child(button(
                    cx,
                    "settings-back",
                    "← 会話に戻る",
                    Tone::Plain,
                    true,
                    |this, window, cx| this.return_to_chat(window, cx),
                ))
                .child(
                    div()
                        .text_lg()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("設定"),
                )
                .into_any_element();
        }
        let name = self
            .model
            .companion()
            .map(|companion| companion.name.clone())
            .unwrap_or_else(|| "コンパニオン".to_owned());
        div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(56.))
            .flex_none()
            .px_4()
            .gap_3()
            .justify_between()
            .border_b_1()
            .border_color(line())
            .bg(card())
            .child(
                div()
                    .id("open-roster")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(rgb(0xf0efec)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.roster_open = true;
                        cx.notify();
                    }))
                    .child(portrait(24.))
                    .child(div().text_sm().truncate().child(name))
                    .child(div().text_xs().text_color(muted()).child("▾")),
            )
            .child(button(
                cx,
                "open-settings",
                "設定",
                Tone::Plain,
                true,
                |this, _window, cx| this.show_settings(SettingsSection::Companion, cx),
            ))
            .into_any_element()
    }

    /// The companions as a team list: face, name, and the one line that says
    /// what this one is for.
    fn render_roster(&self, cx: &mut Context<Self>) -> AnyElement {
        let companions = self
            .model
            .snapshot()
            .map(|snapshot| snapshot.companions.clone())
            .unwrap_or_default();
        let selected = self.model.companion_id().map(str::to_owned);
        let mut rows: Vec<AnyElement> = Vec::new();
        for companion in companions {
            let id = companion.id.clone();
            let active = selected.as_deref() == Some(id.as_str());
            let line = one_line(companion.persona.as_deref().unwrap_or(""));
            rows.push(
                div()
                    .id(SharedString::from(format!("roster-{}", companion.id)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(active, |this| this.bg(rgb(0xf0efec)))
                    .when(!active, |this| this.hover(|this| this.bg(rgb(0xf5f5f4))))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.select(&id, cx);
                        this.return_to_chat(window, cx);
                    }))
                    .child(portrait(34.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .child(div().text_sm().child(companion.name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted())
                                    .child(SharedString::from(line)),
                            ),
                    )
                    .when(active, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(muted())
                                .child("会話中"),
                        )
                    })
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_2()
                    .text_sm()
                    .text_color(muted())
                    .child("コンパニオンがいません")
                    .into_any_element(),
            );
        }
        let panel = div()
            .flex()
            .flex_col()
            .w(px(300.))
            .h_full()
            .bg(rgb(0xffffff))
            .border_r_1()
            .border_color(rgb(0xecebe8))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .px_3()
                    .pt_4()
                    .pb_2()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(muted())
                            .child("コンパニオン"),
                    )
                    .child(button(
                        cx,
                        "close-roster",
                        "閉じる",
                        Tone::Plain,
                        true,
                        |this, window, cx| this.return_to_chat(window, cx),
                    )),
            )
            .child(
                div()
                    .id("roster-list")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_1()
                    .p_2()
                    .children(rows),
            )
            .child(div().p_3().border_t_1().border_color(line()).child(button(
                cx,
                "roster-new",
                "＋ 新しいコンパニオン",
                Tone::Plain,
                true,
                |this, _window, cx| this.show_settings(SettingsSection::New, cx),
            )));
        let scrim = div()
            .id("roster-scrim")
            .flex_1()
            .h_full()
            .bg(rgba(0x1c191740))
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                this.roster_open = false;
                cx.notify();
            }));
        div()
            .id("roster-overlay")
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w_full()
            .h_full()
            .flex()
            .flex_row()
            .child(panel)
            .child(scrim)
            .into_any_element()
    }

    /// Whether the host is reachable.
    pub(crate) fn render_link(&self) -> AnyElement {
        let (dot, note) = match &self.model.link {
            Link::Live { .. } => (rgb(0x15803d), "接続中".to_owned()),
            Link::Connecting { .. } => (rgb(0xb45309), "接続しています".to_owned()),
            Link::Lost { retry_in, .. } => (rgb(0xb91c1c), format!("切断・{retry_in:?}後に再試行")),
            Link::Unset { .. } => (rgb(0xb91c1c), "接続情報がありません".to_owned()),
            Link::Unusable { .. } => (rgb(0xb91c1c), "設定が不正です".to_owned()),
        };
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(card())
            .border_1()
            .border_color(line())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(7.)).rounded_full().bg(dot))
                    .child(div().text_sm().child(format!("ホスト接続　{note}"))),
            )
            .into_any_element()
    }

    fn show_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.roster_open = false;
        self.panel = Panel::Settings;
        self.settings_section = section;
        self.open_panel(cx);
        cx.notify();
    }

    fn return_to_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.roster_open = false;
        self.panel = Panel::Chat;
        focus_chat(self, window, cx);
        cx.notify();
    }

    fn open_panel(&mut self, cx: &mut Context<Self>) {
        match self.panel {
            Panel::Chat => {
                self.load_history(cx);
                self.load_approvals(cx);
            }
            Panel::Settings => {
                self.load_providers(cx);
                self.load_models(cx);
            }
        }
    }
}

/// A companion's face: a head and shoulders cut out of a disc.
fn portrait(size: f32) -> AnyElement {
    div()
        .relative()
        .flex_none()
        .w(px(size))
        .h(px(size))
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(0xe7e5e4))
        .child(
            div()
                .absolute()
                .top(px(size * 0.22))
                .left(px(size * 0.34))
                .size(px(size * 0.32))
                .rounded_full()
                .bg(rgb(0x1c1917)),
        )
        .child(
            div()
                .absolute()
                .bottom(px(-size * 0.18))
                .left(px(size * 0.11))
                .w(px(size * 0.78))
                .h(px(size * 0.55))
                .rounded_full()
                .bg(rgb(0x1c1917)),
        )
        .into_any_element()
}

/// The one line a companion list shows under the name.
fn one_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut chars = line.chars();
    let mut out: String = chars.by_ref().take(30).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    if out.is_empty() {
        "話す相手".to_owned()
    } else {
        out
    }
}
/// A control. A disabled control says it is disabled instead of doing nothing.
pub(crate) fn button<F>(
    cx: &mut Context<Desktop>,
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
    on_click: F,
) -> AnyElement
where
    F: Fn(&mut Desktop, &mut Window, &mut Context<Desktop>) + 'static,
{
    let (background, foreground) = match (tone, enabled) {
        (Tone::Primary, true) => (rgb(0x2563eb), rgb(0xffffff)),
        (Tone::Primary, false) => (rgb(0xe7e5e4), rgb(0x6b625d)),
        (Tone::Plain, true) => (rgb(0xe7e5e4), rgb(0x1c1917)),
        (Tone::Plain, false) => (rgb(0xf5f5f4), rgb(0xa8a29e)),
        (Tone::Danger, true) => (rgb(0xfee2e2), rgb(0x991b1b)),
        (Tone::Danger, false) => (rgb(0xfafaf9), rgb(0xa8a29e)),
    };
    let element = div()
        .id(id)
        .flex_none()
        .px_3()
        .py_2()
        .rounded_md()
        .bg(background)
        .text_color(foreground)
        .text_sm()
        .child(label.into());
    if enabled {
        // Press, not click: the action is the press, so a drag that ends outside
        // the button still counts, and the pointer does not have to be tracked
        // back to the same element to commit.
        element
            .cursor_pointer()
            .hover(|this| {
                this.bg(gpui::Rgba {
                    a: 0.78,
                    ..background
                })
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    on_click(this, window, cx)
                }),
            )
            .into_any_element()
    } else {
        element.into_any_element()
    }
}

/// A local decoding failure is the client's own problem, not the host's.
pub(crate) fn decode(error: buffa::DecodeError) -> connectrpc::ConnectError {
    connectrpc::ConnectError::internal(error.to_string())
}

/// What a failed request means to the operator.
pub(crate) fn failure(context: &'static str, error: connectrpc::ConnectError) -> Update {
    match error.code {
        // Every revision-based call answers a mismatch with `aborted`, so the
        // window refetches the authority instead of guessing.
        connectrpc::ErrorCode::Aborted => Update::Conflict {
            context,
            message: error.to_string(),
        },
        connectrpc::ErrorCode::Unauthenticated | connectrpc::ErrorCode::PermissionDenied => {
            Update::Denied {
                context,
                message: error.to_string(),
            }
        }
        _ => Update::Failed {
            context,
            message: error.to_string(),
        },
    }
}

/// Registers the window's key bindings and action handlers.
pub fn bind(cx: &mut App, view: gpui::WeakEntity<Desktop>) {
    input::bind(cx);
    cx.bind_keys([gpui::KeyBinding::new("enter", Submit, Some("ChatInput"))]);
    cx.bind_keys([gpui::KeyBinding::new("escape", StopPlayback, None)]);
    let submitted = view.clone();
    cx.on_action(move |_: &Submit, cx: &mut App| {
        let _ = submitted.update(cx, |this, cx| this.submit(cx));
    });
    cx.on_action(move |_: &StopPlayback, cx: &mut App| {
        let _ = view.update(cx, |this, cx| {
            // Escape dismisses the roster first; only a conversation-level
            // Escape reaches the turn.
            if this.roster_open {
                this.roster_open = false;
                cx.notify();
            } else {
                this.stop(cx);
            }
        });
    });
}

/// Background colour for an inset card.
pub(crate) fn card() -> gpui::Rgba {
    rgb(0xffffff)
}

/// Border colour for a card or a divider.
pub(crate) fn line() -> gpui::Rgba {
    rgb(0xe7e5e4)
}

/// Muted text.
pub(crate) fn muted() -> gpui::Rgba {
    rgb(0x6b625d)
}

/// Puts the caret in the message field when the window opens, so typing works
/// without a click.
pub fn focus_chat(desktop: &Desktop, window: &mut Window, cx: &mut App) {
    let focus = desktop.chat_input.read(cx).focus_handle();
    window.focus(&focus);
}
