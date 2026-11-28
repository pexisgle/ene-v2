use super::*;

impl Window {
    fn render_chat(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_4()
            .gap_3()
            .child(self.transcript(cx))
            .children(self.approval_cards(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input)))
                    .child(
                        Button::new("send")
                            .label("送信")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.send_text(cx))),
                    ),
            )
            .child(self.voice_controls(cx))
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

    fn transcript(&self, cx: &Context<Self>) -> impl IntoElement {
        let transcript = self.history.get(&self.companion);
        let messages = transcript
            .into_iter()
            .flat_map(|history| history.messages.values())
            .filter(|message| message.role != w::message::Role::Trigger)
            .rev()
            .take(if self.compact { 1 } else { usize::MAX })
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
                            .child(if own {
                                div().child(message.text.clone()).into_any_element()
                            } else {
                                gpui::component::text::TextView::markdown(
                                    message.id.clone(),
                                    message.text.clone(),
                                )
                                .selectable(true)
                                .image_source(|_| {
                                    gpui::ImageSource::Custom(std::sync::Arc::new(|_, _| None))
                                })
                                .on_link_click(|url, _, _, cx| {
                                    if url.starts_with("https://") || url.starts_with("http://") {
                                        cx.open_url(url);
                                    }
                                })
                                .into_any_element()
                            }),
                    )
            });
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
            .children(messages)
            .children(
                transcript
                    .and_then(|timeline| timeline.next_before)
                    .filter(|before| !self.compact && *before != 0)
                    .map(|before| {
                        Button::new("older-history")
                            .label("以前の会話を表示")
                            .on_click(cx.listener(move |owner, _, _, _| {
                                owner.submit(Action::History(owner.companion.clone(), before));
                            }))
                    }),
            )
    }

    fn approval_cards(&self, cx: &Context<Self>) -> Vec<gpui::AnyElement> {
        self.snapshot
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
                    .into_any_element()
            })
            .collect()
    }

    fn voice_controls(&self, cx: &Context<Self>) -> impl IntoElement {
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
            )
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
                    this.submit(Action::History(companion.id.clone(), 0));
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
                        Button::new("compact")
                            .label(if self.compact {
                                "履歴を展開"
                            } else {
                                "コンパクト表示"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.compact = !this.compact;
                                let (width, height) = if this.compact {
                                    (444., 320.)
                                } else {
                                    (540., 620.)
                                };
                                window.resize(gpui::size(px(width), px(height)));
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("settings")
                            .label("設定")
                            .on_click(cx.listener(|this, _, _, cx| this.open_settings(cx))),
                    ),
            );
        let main = if let Some(companion) = self
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
                    && !self.companion.is_empty()
                    && !self.snapshot.activations.iter().any(|active| {
                        active.companion_id == self.companion && active.device_id == self.local.id
                    }),
                |view| {
                    view.child(
                        div().px_4().py_2().child(
                            Button::new("active")
                                .label("この端末をアクティブにする")
                                .on_click(cx.listener(|this, _, _, _| {
                                    this.submit(Action::Activate(this.companion.clone()));
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
