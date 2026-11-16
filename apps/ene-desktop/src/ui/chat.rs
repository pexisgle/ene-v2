//! The conversation panel: history, the live preview, and the composer.

use super::{Desktop, Tone, button, card, decode, line, muted};
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
        self.load_mind(cx);
        cx.notify();
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
            log.push(note("まだ会話がありません"));
        }
        let companion_name = self
            .model
            .companion()
            .map(|companion| companion.name.clone())
            .unwrap_or_default();
        for message in &chat.messages {
            let mine = message.role == w::MessageRole::User;
            let author = match message.role.as_known() {
                Some(w::MessageRole::User) => "あなた".to_owned(),
                Some(w::MessageRole::Assistant) => companion_name.clone(),
                Some(w::MessageRole::Agent) => "エージェント".to_owned(),
                _ => "文脈".to_owned(),
            };
            log.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .when(mine, |this| this.items_end())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .when(mine, |this| this.flex_row_reverse())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted())
                                    .child(SharedString::from(author)),
                            )
                            .child(copy_button(cx, message.text.clone())),
                    )
                    .child(
                        div()
                            .max_w(px(560.))
                            .px_3()
                            .py_2()
                            .rounded_lg()
                            .when(mine, |this| {
                                this.bg(rgb(0x1c1917)).text_color(rgb(0xfaf9f7))
                            })
                            .when(!mine, |this| {
                                this.bg(rgb(0xffffff))
                                    .border_1()
                                    .border_color(rgb(0xecebe8))
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
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(card())
                    .child(div().text_xs().text_color(muted()).child("応答中"))
                    .child(div().text_sm().text_color(muted()).child(preview))
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

        let turn = self
            .model
            .chat
            .turn
            .as_ref()
            .map(|turn| match turn.status.as_known() {
                Some(w::TurnStatus::Streaming) => "応答中".to_owned(),
                Some(w::TurnStatus::Answered) => "応答が完了しました".to_owned(),
                Some(w::TurnStatus::Stopped) => "停止しました".to_owned(),
                Some(w::TurnStatus::Failed) => format!(
                    "失敗しました: {}",
                    turn.failure
                        .as_option()
                        .map(|failure| failure.message.clone())
                        .unwrap_or_default()
                ),
                _ => "状態不明".to_owned(),
            });
        let running = turn.is_some();
        let stopping = self.model.chat.stop_requested;
        let input = self.chat_input.clone();
        let draft_text = input.read(cx).text().to_owned();
        let too_long = draft_text.chars().count() > MAX_INPUT;
        let can_send = self.live() && !draft_text.trim().is_empty() && !too_long;

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
                            .child(input)
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(if too_long {
                                                rgb(0xb91c1c)
                                            } else {
                                                muted()
                                            })
                                            .child(match (turn, stopping) {
                                                (_, true) => {
                                                    "停止を要求しました。ホストの確認待ちです。"
                                                        .to_owned()
                                                }
                                                (Some(turn), false) => turn,
                                                (None, false) if too_long => format!(
                                                    "{} 文字を超えています（上限 {MAX_INPUT}）",
                                                    draft_text.chars().count()
                                                ),
                                                (None, false) => String::new(),
                                            }),
                                    )
                                    .when(stopping || running, |this| {
                                        this.child(button(
                                            cx,
                                            "chat-stop",
                                            "停止",
                                            Tone::Danger,
                                            self.live(),
                                            |this, _window, cx| this.stop(cx),
                                        ))
                                    })
                                    .child(button(
                                        cx,
                                        "chat-send",
                                        "送信",
                                        Tone::Primary,
                                        can_send,
                                        |this, _window, cx| this.submit(cx),
                                    )),
                            ),
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

    /// The cards that belong to the conversation itself: a pending confirmation and
    /// a question a run is waiting on, plus one quiet line while work is running.
    ///
    /// The design routes every approval card through the companion's window, and the
    /// work agents behind it are never named to the user. So these are drawn in the
    /// transcript, phrased as the companion's own ask, and only as much of the host's
    /// record as the user needs to decide.
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
                .child(div().text_xs().text_color(rgb(0x92400e)).child("確認"))
                .child(
                    div()
                        .text_sm()
                        .child(format!("「{what}」を実行してもいい？")),
                );
            if !approval.normalized_arguments.trim().is_empty() {
                card = card.child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0xfff7ed))
                        .text_xs()
                        .text_color(rgb(0x78350f))
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
                            "承認",
                            Tone::Primary,
                            self.live(),
                            move |this, _window, cx| {
                                this.resolve(&approve, w::ApprovalDecision::Approve, cx)
                            },
                        ))
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-always-{always}")),
                            "常に承認",
                            Tone::Plain,
                            self.live(),
                            move |this, _window, cx| {
                                this.resolve(&always, w::ApprovalDecision::AlwaysApprove, cx)
                            },
                        ))
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-deny-{deny}")),
                            "拒否",
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
        let views = self.model.works.clone();
        for view in &views {
            for inquiry in &view.pending_inquiries {
                let request_id = inquiry.request_id.clone();
                let answer_id = inquiry.request_id.clone();
                let work_id = view
                    .session
                    .as_option()
                    .map(|session| session.id.clone())
                    .unwrap_or_default();
                let draft = self.draft(
                    cx,
                    format!("answer:{request_id}"),
                    "回答を入力",
                    "TextField",
                );
                let choices = if inquiry.choices.is_empty() {
                    String::new()
                } else {
                    format!("（{}）", inquiry.choices.join(" / "))
                };
                cards.push(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .p_4()
                        .rounded_lg()
                        .bg(rgb(0xeff6ff))
                        .border_1()
                        .border_color(rgb(0xbfdbfe))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x1d4ed8))
                                .child("作業からの確認"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .child(format!("{}{choices}", inquiry.prompt)),
                        )
                        .child(draft)
                        .child(button(
                            cx,
                            SharedString::from(format!("inline-answer-{answer_id}")),
                            "回答",
                            Tone::Primary,
                            self.live(),
                            move |this, _window, cx| {
                                this.answer(work_id.clone(), answer_id.clone(), cx)
                            },
                        ))
                        .into_any_element(),
                );
            }
        }
        let running = views.iter().find_map(|view| {
            let session = view.session.as_option()?;
            (session.status == w::WorkStatus::Active).then(|| session.id.clone())
        });
        if let Some(work_id) = running {
            cards.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .text_xs()
                    .text_color(muted())
                    .child("作業を進めています")
                    .child(button(
                        cx,
                        SharedString::from(format!("inline-stop-work-{work_id}")),
                        "止める",
                        Tone::Plain,
                        self.live(),
                        move |this, _window, cx| this.stop_work(work_id.clone(), cx),
                    ))
                    .into_any_element(),
            );
        }
        cards
    }
}

/// Copies a message's text. The bubbles are plain text, not a selectable
/// document, so this is the only way to take a reply out of the conversation.
fn copy_button(cx: &mut Context<Desktop>, text: String) -> AnyElement {
    let id = SharedString::from(format!("copy-{}", text.len()));
    div()
        .id(id)
        .px_1()
        .text_xs()
        .text_color(muted())
        .cursor_pointer()
        .hover(|this| this.text_color(rgb(0x1c1917)))
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(move |_this, _: &gpui::MouseDownEvent, _window, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
            }),
        )
        .child("コピー")
        .into_any_element()
}
