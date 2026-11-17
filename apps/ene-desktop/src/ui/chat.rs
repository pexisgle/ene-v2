//! The conversation panel: history, the live preview, and the composer.

use super::{Desktop, SettingsSection, Tone, button, card, decode, line, muted};
use crate::{
    state::{Chat, NoticeKind},
    update::Update,
};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px, rgb};

/// The host refuses a message longer than this (`host/chat.rs`).
const MAX_INPUT: usize = 32_000;

impl Desktop {
    /// Switches the shown companion and asks the session to serve it.
    pub(crate) fn select(&mut self, companion_id: &str, cx: &mut Context<Self>) {
        if self.model.companion_id() == Some(companion_id) {
            return;
        }
        self.model.selected = Some(companion_id.to_owned());
        self.refresh_placeholder(cx);
        self.model.chat = Chat {
            companion_id: companion_id.to_owned(),
            loading: true,
            ..Default::default()
        };
        self.model.streaming.clear();
        // The activation is per companion, so the session releases the previous
        // one and takes the new one; audio and perception follow it.
        let _ = self
            .commands
            .send(crate::update::Command::Serve(companion_id.to_owned()));
        self.load_history(cx);
        cx.notify();
    }

    /// Keeps the composer's hint pointed at the companion on screen.
    pub(crate) fn refresh_placeholder(&mut self, cx: &mut Context<Self>) {
        let placeholder = match self.model.companion() {
            Some(companion) if !companion.name.is_empty() => {
                format!("{}に話しかける", companion.name)
            }
            _ => "メッセージを入力（Enterで送信）".to_owned(),
        };
        self.chat_input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
    }
    /// Fetches the newest page of the shown conversation.
    pub(crate) fn load_history(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        if !self.live() {
            // Without a link the window keeps what it has and says it is stale;
            // it never presents the old page as current.
            self.model.chat.loading = false;
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        if self.model.chat.companion_id != companion_id {
            self.model.chat = Chat {
                companion_id: companion_id.clone(),
                loading: true,
                ..Default::default()
            };
        }
        self.model.chat.loading = true;
        let shown = companion_id.clone();
        self.request(
            "会話履歴の取得",
            async move {
                // The host pages by "newer than" only and returns the newest
                // page, so the window asks for the newest page and nothing else.
                let response = host
                    .chat
                    .get_history(w::HistoryRequest {
                        companion_id: companion_id.clone(),
                        limit: Some(200),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |history| Update::History {
                companion_id: shown,
                messages: history.items,
            },
        );
    }

    pub(crate) fn render_chat(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut log: Vec<AnyElement> = Vec::new();
        let chat = &self.model.chat;
        if chat.loading && chat.messages.is_empty() {
            log.push(note("履歴を取得しています"));
        }
        if let Some(error) = chat.error.as_ref() {
            log.push(note(&format!("履歴を取得できませんでした: {error}")));
        }
        if !chat.loading && chat.messages.is_empty() && chat.error.is_none() {
            let (heading, hint) = match self.model.companion() {
                _ if !self.live() => (
                    "ホストに接続していません".to_owned(),
                    "接続状態は「この端末」で確認できます。",
                ),
                Some(companion) => (
                    format!("{} との会話", companion.name),
                    "下の入力欄から話しかけてみましょう。",
                ),
                None => (
                    "最初のコンパニオンを作りましょう".to_owned(),
                    "名前と会話に使うモデルを選ぶと、話しかけられます。",
                ),
            };
            let action = if !self.live() {
                Some(("端末の状態を確認", SettingsSection::Device))
            } else if self.model.companion().is_none() {
                Some(("コンパニオンを作成", SettingsSection::New))
            } else {
                None
            };
            log.push(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .py_8()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(heading),
                    )
                    .child(note(hint))
                    .when_some(action, |this, (label, section)| {
                        this.child(button(
                            cx,
                            "chat-empty-action",
                            label,
                            Tone::Plain,
                            true,
                            move |this, _window, cx| this.show_settings(section, cx),
                        ))
                    })
                    .into_any_element(),
            );
        } else if !self.live() {
            log.insert(
                0,
                button(
                    cx,
                    "chat-offline",
                    "接続状態を確認",
                    Tone::Plain,
                    true,
                    |this, _window, cx| this.show_settings(SettingsSection::Device, cx),
                ),
            );
        }
        for message in &chat.messages {
            let mine = message.role == w::MessageRole::User;
            log.push(
                div()
                    .flex()
                    .flex_col()
                    .when(mine, |this| this.items_end())
                    .when(!mine, |this| this.items_start())
                    .child(
                        div()
                            .max_w(px(560.))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .when(mine, |this| this.bg(rgb(0xecebe8)))
                            .when(!mine, |this| {
                                this.bg(card()).border_1().border_color(rgb(0xecebe8))
                            })
                            .text_sm()
                            .child(SharedString::from(message.text.clone())),
                    )
                    .into_any_element(),
            );
        }
        // A preview is drawn as a preview: dimmed, and never stored as a
        // message until the host records one.
        let preview: String = self
            .model
            .streaming
            .iter()
            .filter(|((companion, _), _)| companion == &self.model.chat.companion_id)
            .map(|(_, text)| text.as_str())
            .collect();
        if !preview.is_empty() {
            log.push(
                div()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(
                        div()
                            .max_w(px(560.))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .bg(card())
                            .border_1()
                            .border_color(rgb(0xecebe8))
                            .text_sm()
                            .text_color(muted())
                            .child(preview),
                    )
                    .into_any_element(),
            );
        }

        let mut unsent: Vec<AnyElement> = Vec::new();
        for (index, item) in self.model.chat.unsent.iter().enumerate() {
            let key = item.request_key.clone();
            let text = item.text.clone();
            unsent.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0xfef2f2))
                    .child(
                        div()
                            .flex_col()
                            .flex_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x991b1b))
                                    .child(item.reason.clone()),
                            )
                            .child(div().text_sm().child(text.clone())),
                    )
                    .child(button(
                        cx,
                        SharedString::from(format!("retry-{index}")),
                        "再送",
                        Tone::Primary,
                        self.live(),
                        move |this, _window, cx| this.retry(&key, &text, cx),
                    ))
                    .into_any_element(),
            );
        }

        let input = self.chat_input.clone();
        let draft_text = input.read(cx).text().to_owned();
        let too_long = draft_text.chars().count() > MAX_INPUT;
        let can_send = self.live()
            && self.model.companion_id().is_some()
            && !draft_text.trim().is_empty()
            && !too_long;

        let cards = self.inline_cards(cx);
        div()
            .flex()
            .flex_col()
            .flex_1()
            .overflow_hidden()
            .child(
                div()
                    .id("chat-log")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .w_full()
                            .max_w(px(760.))
                            .mx_auto()
                            .px_5()
                            .py_4()
                            .children(log)
                            .children(cards),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .border_t_1()
                    .border_color(line())
                    .bg(rgb(0xffffff))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .w_full()
                            .max_w(px(760.))
                            .mx_auto()
                            .children(unsent)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(div().flex_1().child(input))
                                    .child(button(
                                        cx,
                                        "chat-send",
                                        "送信",
                                        Tone::Primary,
                                        can_send,
                                        |this, _window, cx| this.submit(cx),
                                    )),
                            )
                            .when(too_long, |this| {
                                this.child(div().text_xs().text_color(rgb(0xb91c1c)).child(
                                    format!(
                                        "{} 文字を超えています（上限 {MAX_INPUT}）",
                                        draft_text.chars().count()
                                    ),
                                ))
                            }),
                    ),
            )
            .into_any_element()
    }

    /// Resends text with its original key, so the host cannot record it twice.
    fn retry(&mut self, request_key: &str, text: &str, cx: &mut Context<Self>) {
        self.model
            .chat
            .unsent
            .retain(|item| item.request_key != request_key);
        self.send_keyed(text.to_owned(), request_key.to_owned(), cx);
    }
}

fn note(text: &str) -> AnyElement {
    div()
        .px_3()
        .py_2()
        .text_sm()
        .text_color(muted())
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

/// The composer's entry point from the key binding.
impl Desktop {
    pub(crate) fn submit(&mut self, cx: &mut Context<Self>) {
        tracing::debug!(
            composing = self.chat_input.read(cx).composing(),
            draft = self.chat_input.read(cx).text().chars().count(),
            "submit"
        );
        if self.chat_input.read(cx).composing() {
            // The composition is still open, so this Enter belongs to the IME.
            return;
        }
        let text = self.chat_input.read(cx).text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if text.chars().count() > MAX_INPUT {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                format!("{MAX_INPUT} 文字を超えるメッセージは送信できません"),
            );
            cx.notify();
            return;
        }
        self.send(text, cx);
    }

    /// The one card that belongs to the conversation itself: a pending
    /// confirmation, drawn as the companion's own ask. Work happens behind the
    /// dialogue and has no card of its own.
    fn inline_cards(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut cards: Vec<AnyElement> = Vec::new();
        let approvals = self.model.approvals().to_vec();
        for approval in approvals {
            let approve = approval.request_id.clone();
            let always = approval.request_id.clone();
            let deny = approval.request_id.clone();
            let what = approval
                .capability
                .clone()
                .filter(|capability| !capability.is_empty())
                .unwrap_or_else(|| approval.tool_name.clone());
            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .rounded_lg()
                .bg(rgb(0xfffbeb))
                .border_1()
                .border_color(rgb(0xfde68a))
                .child(
                    div()
                        .text_sm()
                        .child(format!("「{what}」を実行してもいい？")),
                );
            if !approval.normalized_arguments.trim().is_empty() {
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(muted())
                        .child(approval.normalized_arguments.replace('\n', " ")),
                );
            }
            cards.push(
                card.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-approve-{approve}")),
                            "いいよ",
                            Tone::Primary,
                            self.live(),
                            move |this, _window, cx| {
                                this.resolve(&approve, w::ApprovalDecision::Approve, cx)
                            },
                        ))
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-always-{always}")),
                            "いつも許可する",
                            Tone::Plain,
                            self.live(),
                            move |this, _window, cx| {
                                this.resolve(&always, w::ApprovalDecision::AlwaysApprove, cx)
                            },
                        ))
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-deny-{deny}")),
                            "やめて",
                            Tone::Danger,
                            self.live(),
                            move |this, _window, cx| {
                                this.resolve(&deny, w::ApprovalDecision::Deny, cx)
                            },
                        )),
                )
                .into_any_element(),
            );
        }
        cards
    }
}
