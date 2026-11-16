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
mod work;

use crate::{
    input::{self, Draft},
    session::{Connection, Options},
    state::{Device, Link, Model, NoticeKind},
    update::{Command, Update},
};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{
    AnyElement, App, ClickEvent, ElementId, Entity, Render, SharedString, Task, Window, div,
    prelude::*, px, rgb,
};
use std::collections::BTreeMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

gpui::actions!(ene, [Submit, StopPlayback]);

/// Which surface the window is showing. Approvals and work are not surfaces of
/// their own: the design consolidates the approval card into the companion's
/// window, and the user is never shown the work agents behind it.
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
            SettingsSection::Connections => "接続とモデル",
            SettingsSection::Voice => "声",
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
    pub(crate) settings_section: SettingsSection,
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
            settings_section: SettingsSection::Companion,
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
        let had_approvals = self.model.approvals().len();
        let effects = self.model.apply(update);
        // The link goes live before the snapshot names a companion, so the
        // first fetch can run with nothing selected and never repeat.
        let selected_now = !had_companion && self.model.companion_id().is_some();
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
        if effects.works || selected_now {
            self.load_works(cx);
        }
        if effects.approvals || selected_now {
            self.load_approvals(cx);
        }
        if effects.mind || selected_now {
            self.load_mind(cx);
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
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0xfaf9f7))
            .text_color(rgb(0x1c1917))
            .font_family("Yu Gothic UI")
            .child(self.render_sidebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_hidden()
                    .child(match self.panel {
                        Panel::Chat => self.render_chat(cx),
                        Panel::Settings => self.render_settings(cx),
                    })
                    .child(self.render_status(cx)),
            )
    }
}

impl Desktop {
    /// The companions and the way between the two surfaces. Everything the
    /// window offers lives here or in the conversation.
    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let companions = self
            .model
            .snapshot()
            .map(|snapshot| snapshot.companions.clone())
            .unwrap_or_default();
        let selected = self.model.companion_id().map(str::to_owned);
        let mut rows: Vec<AnyElement> = Vec::new();
        for companion in companions {
            let is_selected =
                selected.as_deref() == Some(companion.id.as_str()) && self.panel == Panel::Chat;
            let id = companion.id.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!("companion-{}", companion.id)))
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .when(is_selected, |this| {
                        this.bg(rgb(0x1c1917)).text_color(rgb(0xfaf9f7))
                    })
                    .when(!is_selected, |this| {
                        this.text_color(rgb(0x44403c))
                            .hover(|this| this.bg(rgb(0xecebe8)))
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        this.panel = Panel::Chat;
                        this.select(&id, cx);
                    }))
                    .child(companion.name)
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(muted())
                    .child("コンパニオンがいません")
                    .into_any_element(),
            );
        }
        let settings_selected = self.panel == Panel::Settings;
        div()
            .flex()
            .flex_col()
            .w(px(196.))
            .h_full()
            .border_r_1()
            .border_color(rgb(0xecebe8))
            .bg(rgb(0xffffff))
            .p_2()
            .gap_1()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(muted())
                    .child("コンパニオン"),
            )
            .children(rows)
            .child(
                div()
                    .id("new-companion")
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .text_color(rgb(0x2563eb))
                    .cursor_pointer()
                    .hover(|this| this.bg(rgb(0xf5f5f4)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.panel = Panel::Settings;
                        this.settings_section = SettingsSection::New;
                        this.open_panel(cx);
                        cx.notify();
                    }))
                    .child("＋ 新しいコンパニオン"),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("open-settings")
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .text_sm()
                    .when(settings_selected, |this| {
                        this.bg(rgb(0xecebe8)).text_color(rgb(0x1c1917))
                    })
                    .when(!settings_selected, |this| {
                        this.text_color(rgb(0x57534e))
                            .hover(|this| this.bg(rgb(0xf5f5f4)))
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.panel = if this.panel == Panel::Settings {
                            Panel::Chat
                        } else {
                            Panel::Settings
                        };
                        this.open_panel(cx);
                        cx.notify();
                    }))
                    .child(if settings_selected {
                        "会話へ戻る"
                    } else {
                        "設定"
                    }),
            )
            .into_any_element()
    }

    /// One short line: whether the host is reachable, and what this terminal can
    /// do right now. The rest of the host's record belongs to settings.
    fn render_status(&self, _cx: &mut Context<Self>) -> AnyElement {
        let (dot, note) = match &self.model.link {
            Link::Live { .. } => (rgb(0x15803d), "接続中".to_owned()),
            Link::Connecting { .. } => (rgb(0xb45309), "接続しています".to_owned()),
            Link::Lost { retry_in, .. } => (rgb(0xb91c1c), format!("切断・{retry_in:?}後に再試行")),
            Link::Unset { .. } => (rgb(0xb91c1c), "接続情報がありません".to_owned()),
            Link::Unusable { .. } => (rgb(0xb91c1c), "設定が不正です".to_owned()),
        };
        let devices = &self.model.devices;
        let state = |device: &Device| match device {
            Device::Off => "停止",
            Device::Ready(_) => "利用可",
            Device::Unavailable(_) => "利用不可",
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_4()
            .py_1()
            .border_t_1()
            .border_color(rgb(0xecebe8))
            .bg(rgb(0xffffff))
            .text_xs()
            .text_color(muted())
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(note)
            .child(div().flex_1())
            .child(format!("アバター {}", state(&devices.avatar)))
            .child(format!("出力 {}", state(&devices.output)))
            .child(format!("入力 {}", state(&devices.input)))
            .into_any_element()
    }
    fn open_panel(&mut self, cx: &mut Context<Self>) {
        match self.panel {
            Panel::Chat => {
                self.load_history(cx);
                self.load_approvals(cx);
                self.load_works(cx);
            }
            Panel::Settings => {
                self.load_providers(cx);
                self.load_models(cx);
                self.load_mind(cx);
            }
        }
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
        (Tone::Primary, false) => (rgb(0xbfdbfe), rgb(0xffffff)),
        (Tone::Plain, true) => (rgb(0xe7e5e4), rgb(0x1c1917)),
        (Tone::Plain, false) => (rgb(0xf5f5f4), rgb(0xa8a29e)),
        (Tone::Danger, true) => (rgb(0xfee2e2), rgb(0x991b1b)),
        (Tone::Danger, false) => (rgb(0xfafaf9), rgb(0xa8a29e)),
    };
    let element = div()
        .id(id)
        .px_2()
        .py_1()
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
        let _ = view.update(cx, |this, cx| this.stop(cx));
    });
}

/// Background colour for an inset card.
pub(crate) fn card() -> gpui::Rgba {
    rgb(0xffffff)
}

/// A page heading with one line of plain explanation under it.
pub(crate) fn page_title(title: &str, subtitle: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_lg().child(title.to_owned()))
        .child(
            div()
                .text_sm()
                .text_color(muted())
                .child(subtitle.to_owned()),
        )
        .into_any_element()
}
/// Border colour for a card or a divider.
pub(crate) fn line() -> gpui::Rgba {
    rgb(0xe7e5e4)
}

/// Muted text.
pub(crate) fn muted() -> gpui::Rgba {
    rgb(0x78716c)
}

/// Puts the caret in the message field when the window opens, so typing works
/// without a click.
pub fn focus_chat(desktop: &Desktop, window: &mut Window, cx: &mut App) {
    let focus = desktop.chat_input.read(cx).focus_handle();
    window.focus(&focus);
}
