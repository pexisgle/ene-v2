//! Minimal frameless floating companion window: speech bubble, inline approval,
//! first-run onboarding, collapsible history, capsule composer and 3 orb controls.

use super::icons::{Icon, icon};
use super::theme::{Theme, resolve_ui_font_family, ui_font_fallbacks};
use super::widgets::{
    Tone, companion_badge, floating_orb_button, pill_button, status_pill, window_drag_area,
};
use super::{Hub, NewCompanionSpec, PodMode, SettingsSection};
use crate::{
    input::{self, Draft},
    state::{Device, Link, NoticeKind},
};
use ene_rpc as w;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, KeyBinding, MouseButton, MouseDownEvent, Pixels,
    ScrollHandle, SharedString, Size, Subscription, WeakEntity, Window, actions, div, point,
    prelude::*, px,
};

const MAX_INPUT: usize = 4000;

actions!(ene_desktop, [Submit, CancelOrStop]);

/// Compatibility alias expected by `lib.rs`.
pub type Desktop = CompanionPod;

/// Computes default bottom-right floating placement on the primary display.
pub fn initial_pod_bounds(cx: &App) -> Bounds<Pixels> {
    let size = PodMode::MiniVoice.size();
    if let Some(display) = cx.primary_display() {
        let screen = display.bounds();
        let x = (screen.right() - size.width - px(28.)).max(screen.left() + px(16.));
        let y = (screen.bottom() - size.height - px(64.)).max(screen.top() + px(16.));
        Bounds::new(point(x, y), size)
    } else {
        Bounds::centered(None, size, cx)
    }
}

/// Frameless transparent companion dialogue window.
pub struct CompanionPod {
    hub: Entity<Hub>,
    chat_input: Entity<Draft>,
    onboard_key: Entity<Draft>,
    onboard_name: Entity<Draft>,
    selected_provider_id: Option<String>,
    onboarding_step: Option<u8>,
    roster_open: bool,
    last_size: Size<Pixels>,
    title_initialized: bool,
    scroll: ScrollHandle,
    last_scroll_ping: u64,
    _observe: Subscription,
}

impl CompanionPod {
    pub fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let chat_input = cx.new(|cx| Draft::new(cx, "メッセージを入力…", "ChatInput"));
        let onboard_key = cx.new(|cx| {
            let mut draft = Draft::new(cx, "APIキーを入力…", "TextField");
            draft.set_masked(true, cx);
            draft
        });
        let onboard_name = cx.new(|cx| {
            let mut draft = Draft::new(cx, "コンパニオン名", "TextField");
            draft.set("エネ", cx);
            draft
        });
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            chat_input,
            onboard_key,
            onboard_name,
            selected_provider_id: None,
            onboarding_step: None,
            roster_open: false,
            last_size: PodMode::MiniVoice.size(),
            title_initialized: false,
            scroll: ScrollHandle::new(),
            last_scroll_ping: 0,
            _observe,
        }
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        let handle = self.chat_input.read(cx).focus_handle();
        window.focus(&handle);
    }

    fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.hub.read(cx).pod_mode.toggle();
        self.hub.update(cx, |hub, cx| hub.set_pod_mode(next, cx));
        let desired = self.desired_size(cx);
        self.apply_size(desired, window, cx);
        cx.notify();
    }

    fn desired_size(&self, cx: &App) -> Size<Pixels> {
        let hub = self.hub.read(cx);
        if hub.pod_mode == PodMode::ExpandedChat {
            return PodMode::ExpandedChat.size();
        }
        let companions_empty = hub
            .model
            .snapshot()
            .is_none_or(|snap| snap.companions.is_empty());
        let has_llm = hub.model.settings().is_some_and(|settings| {
            settings
                .connections
                .iter()
                .any(|c| c.kind == w::ConnectionKind::Llm && c.enabled)
        });
        let in_onboarding =
            hub.live() && (!has_llm || companions_empty || self.onboarding_step.is_some());
        if in_onboarding {
            let h = if hub.model.notices.is_empty() {
                376.
            } else {
                428.
            };
            gpui::size(px(444.), px(h))
        } else if !hub.model.approvals().is_empty() || self.roster_open {
            gpui::size(px(444.), px(376.))
        } else if hub.model.transcription.is_some()
            || !hub.model.notices.is_empty()
            || hub.model.companion_speaking
            || !hub.model.streaming.is_empty()
        {
            gpui::size(px(444.), px(264.))
        } else {
            PodMode::MiniVoice.size()
        }
    }

    fn apply_size(&mut self, desired: Size<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.last_size != desired {
            #[cfg(windows)]
            let saved_bottom = current_pod_hwnd()
                .and_then(|hwnd| read_window_bottom(hwnd).map(|b| (hwnd.0 as isize, b)));
            self.last_size = desired;
            window.resize(desired);
            #[cfg(windows)]
            if let Some((hwnd_raw, target_bottom)) = saved_bottom {
                cx.spawn(async move |_, _| {
                    anchor_window_bottom(hwnd_raw, target_bottom);
                })
                .detach();
            }
            #[cfg(not(windows))]
            let _ = cx;
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.chat_input.read(cx).composing() {
            return;
        }
        let text = self.chat_input.read(cx).text().trim().to_owned();
        if text.is_empty() || text.chars().count() > MAX_INPUT {
            return;
        }
        let sent = self.hub.update(cx, |hub, cx| hub.send(text, cx));
        if sent {
            self.chat_input.update(cx, |draft, cx| draft.clear(cx));
        }
    }

    fn render_bubble_stage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let theme = hub.theme();
        let live = hub.live();
        let expanded = hub.pod_mode == PodMode::ExpandedChat;
        let companion = hub.model.companion().cloned();
        let companions_empty = hub
            .model
            .snapshot()
            .is_none_or(|snap| snap.companions.is_empty());
        let has_llm = hub.model.settings().is_some_and(|settings| {
            settings
                .connections
                .iter()
                .any(|c| c.kind == w::ConnectionKind::Llm && c.enabled)
        });
        let has_tts = hub.model.settings().is_some_and(|settings| {
            settings
                .connections
                .iter()
                .any(|c| c.kind == w::ConnectionKind::Tts && c.enabled)
        });
        let approvals = hub.model.approvals().to_vec();
        let notices = hub.model.notices.clone();
        let companion_name = companion
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "エネ".to_owned());

        let streaming_preview: String = hub
            .model
            .streaming
            .iter()
            .filter(|((c, _), _)| c == &hub.model.chat.companion_id)
            .map(|(_, text)| text.as_str())
            .collect();
        let speaking = hub.model.companion_speaking;
        let stop_requested = hub.model.chat.stop_requested;
        let is_busy = speaking || !streaming_preview.is_empty() || stop_requested;

        let in_onboarding =
            live && (!has_llm || companions_empty || self.onboarding_step.is_some());

        let status_word = match &hub.model.link {
            Link::Connecting { .. } => "接続中",
            Link::Lost { .. } | Link::Unset { .. } | Link::Unusable { .. } => "未接続",
            Link::Live { .. } if !approvals.is_empty() => "確認待ち",
            Link::Live { .. } if in_onboarding => "初期設定",
            Link::Live { .. } if speaking => "発話中",
            Link::Live { .. } if !streaming_preview.is_empty() => "生成中",
            Link::Live { .. } if hub.model.transcription.is_some() => "音声入力中",
            Link::Live { .. } => "待機中",
        };
        let badge_label = format!("{companion_name} ・ {status_word}");

        let notice_strip = notices.first().cloned().map(|notice| {
            let hub_dismiss = self.hub.clone();
            let hub_resync = self.hub.clone();
            let is_conflict = notice.kind == NoticeKind::Conflict;
            div()
                .w_full()
                .rounded(px(12.))
                .bg(if is_conflict {
                    theme.amber_soft
                } else {
                    theme.danger_soft
                })
                .px(px(10.))
                .py(px(6.))
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(if is_conflict {
                            theme.amber_fg
                        } else {
                            theme.danger_fg
                        })
                        .truncate()
                        .child(format!("{}: {}", notice.context, notice.message)),
                )
                .when(is_conflict, |this| {
                    this.child(pill_button(
                        "pod-notice-reload",
                        "再読み込み",
                        Some(Icon::Refresh),
                        Tone::Soft,
                        true,
                        &theme,
                        move |_, cx| {
                            hub_resync.update(cx, |hub, cx| {
                                hub.resnapshot();
                                hub.refresh_active_data(cx);
                                hub.dismiss_notice(0, cx);
                            });
                        },
                    ))
                })
                .child(
                    div()
                        .id("pod-notice-close")
                        .flex_none()
                        .p(px(4.))
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|this| this.bg(theme.bg_surface.alpha(0.6)))
                        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                            hub_dismiss.update(cx, |hub, cx| hub.dismiss_notice(0, cx));
                        })
                        .child(icon(
                            Icon::Close,
                            11.,
                            if is_conflict {
                                theme.amber_fg
                            } else {
                                theme.danger_fg
                            },
                        )),
                )
        });

        let body: AnyElement = if !live {
            self.render_offline_body(&hub.model.link, &theme)
        } else if let Some(approval) = approvals.first().cloned() {
            self.render_approval_body(&approval, approvals.len(), &theme)
        } else if in_onboarding {
            let effective_step = if !has_llm {
                0
            } else if self.onboarding_step == Some(1) {
                1
            } else if companions_empty {
                self.onboarding_step.unwrap_or(1).max(1)
            } else {
                2
            };
            self.render_onboarding_body(effective_step, has_tts, companions_empty, &theme, cx)
        } else if expanded {
            self.render_expanded_timeline(&streaming_preview, is_busy, stop_requested, &theme, cx)
        } else {
            self.render_mini_body(&streaming_preview, is_busy, stop_requested, &theme, cx)
        };

        let drag_strip = window_drag_area(
            div()
                .w_full()
                .h(if expanded { px(14.) } else { px(6.) })
                .flex()
                .flex_row()
                .items_center()
                .justify_end(),
            false,
        )
        .when(expanded, |this| {
            this.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.ink_placeholder)
                    .child("履歴"),
            )
        });

        let badge = div()
            .absolute()
            .top(px(0.))
            .left(px(16.))
            .px(px(12.))
            .py(px(4.))
            .rounded_full()
            .bg(theme.accent)
            .border_2()
            .border_color(theme.bg_surface)
            .text_color(theme.accent_fg)
            .text_xs()
            .font_weight(gpui::FontWeight::BOLD)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(5.))
            .child(icon(Icon::Sparkles, 12., theme.accent_fg))
            .child(badge_label);

        let bubble = div()
            .w_full()
            .when(expanded, |this| this.flex_1().min_h_0())
            .bg(theme.bg_surface)
            .border_2()
            .border_color(theme.border_float)
            .shadow(theme.float_shadow())
            .rounded_tl(px(24.))
            .rounded_tr(px(24.))
            .rounded_bl(px(24.))
            .rounded_br(px(8.))
            .px(px(16.))
            .pt(px(12.))
            .pb(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(drag_strip)
            .when_some(notice_strip, |this, strip| this.child(strip))
            .child(body);

        let tail_dot = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .pr(px(12.))
            .child(window_drag_area(div().flex_1().h(px(12.)), false))
            .child(
                div()
                    .size(px(12.))
                    .rounded_full()
                    .bg(theme.bg_surface)
                    .border_2()
                    .border_color(theme.border_float)
                    .shadow(theme.float_shadow()),
            );

        div()
            .relative()
            .w_full()
            .when(expanded, |this| this.flex_1().min_h_0())
            .pt(px(14.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(bubble)
            .child(tail_dot)
            .child(badge)
            .into_any_element()
    }

    fn render_offline_body(&self, link: &Link, theme: &Theme) -> AnyElement {
        let (title, detail) = match link {
            Link::Connecting { endpoint, .. } => {
                ("ホストへ接続しています", format!("接続先: {endpoint}"))
            }
            Link::Lost { reason, .. } => ("ホストとの接続が切れました", reason.clone()),
            Link::Unset { reason, .. } => ("接続先が見つかりません", reason.clone()),
            Link::Unusable { reason } => ("接続設定を確認してください", reason.clone()),
            Link::Live { .. } => ("接続中", String::new()),
        };
        let hub_settings = self.hub.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child(title),
            )
            .when(!detail.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .line_clamp(2)
                        .child(detail),
                )
            })
            .child(div().flex().flex_row().pt(px(2.)).child(pill_button(
                "pod-offline-device",
                "接続を確認",
                Some(Icon::Plug),
                Tone::Soft,
                true,
                theme,
                move |_, cx| {
                    hub_settings.update(cx, |hub, cx| {
                        hub.open_settings(SettingsSection::Device, cx);
                    });
                },
            )))
            .into_any_element()
    }

    fn render_approval_body(
        &self,
        approval: &w::ApprovalRequest,
        total_pending: usize,
        theme: &Theme,
    ) -> AnyElement {
        let what = approval
            .capability
            .as_deref()
            .filter(|c| !c.is_empty())
            .unwrap_or(&approval.tool_name)
            .to_owned();
        let args = approval.normalized_arguments.trim().replace('\n', " ");
        let req_id_a = approval.request_id.clone();
        let req_id_b = approval.request_id.clone();
        let req_id_c = approval.request_id.clone();
        let hub_a = self.hub.clone();
        let hub_b = self.hub.clone();
        let hub_c = self.hub.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child(format!("「{what}」の実行を許可しますか？")),
                    )
                    .when(total_pending > 1, |this| {
                        this.child(status_pill(
                            format!("他{}件", total_pending - 1),
                            None,
                            theme.amber_soft,
                            theme.amber_fg,
                        ))
                    }),
            )
            .when(!args.is_empty(), |this| {
                this.child(
                    div()
                        .rounded(px(10.))
                        .bg(theme.bg_blush)
                        .px(px(10.))
                        .py(px(6.))
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .line_clamp(2)
                        .child(args),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(6.))
                    .pt(px(2.))
                    .child(pill_button(
                        SharedString::from(format!("pod-approve-{}", approval.request_id)),
                        "許可",
                        Some(Icon::Check),
                        Tone::Primary,
                        true,
                        theme,
                        move |_, cx| {
                            hub_a.update(cx, |hub, cx| {
                                hub.resolve_approval(&req_id_a, w::ApprovalDecision::Approve, cx);
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("pod-always-{}", approval.request_id)),
                        "常に許可",
                        None,
                        Tone::Soft,
                        true,
                        theme,
                        move |_, cx| {
                            hub_b.update(cx, |hub, cx| {
                                hub.resolve_approval(
                                    &req_id_b,
                                    w::ApprovalDecision::AlwaysApprove,
                                    cx,
                                );
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("pod-deny-{}", approval.request_id)),
                        "拒否",
                        Some(Icon::Close),
                        Tone::Danger,
                        true,
                        theme,
                        move |_, cx| {
                            hub_c.update(cx, |hub, cx| {
                                hub.resolve_approval(&req_id_c, w::ApprovalDecision::Deny, cx);
                            });
                        },
                    )),
            )
            .into_any_element()
    }

    fn render_onboarding_body(
        &self,
        step: u8,
        has_tts: bool,
        companions_empty: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let step_pill = |idx: u8, title: &'static str| -> AnyElement {
            let done = step > idx;
            let current = step == idx;
            let (bg, fg) = if done {
                (theme.mint_soft, theme.mint_fg)
            } else if current {
                (theme.accent_soft, theme.accent_soft_fg)
            } else {
                (theme.bg_blush, theme.ink_muted)
            };
            status_pill(title, None, bg, fg)
        };

        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .child(step_pill(0, "1 AI接続"))
            .child(step_pill(1, "2 音声（任意）"))
            .child(step_pill(2, "3 対話開始"));

        let body: AnyElement = match step {
            0 => {
                let providers = self
                    .hub
                    .read(cx)
                    .model
                    .providers
                    .clone()
                    .unwrap_or_default();
                let picked = self
                    .selected_provider_id
                    .as_deref()
                    .and_then(|id| providers.iter().find(|p| p.id == id))
                    .or_else(|| providers.iter().find(|p| p.id == "openrouter"))
                    .or_else(|| providers.first())
                    .cloned();

                let mut provider_chips = Vec::new();
                for p in providers.iter().take(5) {
                    let pid = p.id.clone();
                    let prov_needs_key = p.requires_credentials;
                    let active = picked.as_ref().is_some_and(|sel| sel.id == pid);
                    let pod = cx.entity();
                    provider_chips.push(pill_button(
                        SharedString::from(format!("onboard-prov-{}", p.id)),
                        p.name.clone(),
                        active.then_some(Icon::Check),
                        if active { Tone::Primary } else { Tone::Ghost },
                        true,
                        theme,
                        move |_, cx| {
                            pod.update(cx, |this, cx| {
                                this.selected_provider_id = Some(pid.clone());
                                let hint = if prov_needs_key {
                                    "APIキーを入力…"
                                } else {
                                    "APIキー（任意）"
                                };
                                this.onboard_key
                                    .update(cx, |d, cx| d.set_placeholder(hint, cx));
                                cx.notify();
                            });
                        },
                    ));
                }

                let look = theme.field_look(false, false);
                self.onboard_key.update(cx, |d, _| d.apply_look(look));
                let secret = self.onboard_key.read(cx).text().trim().to_owned();
                let needs_key = picked.as_ref().is_some_and(|p| p.requires_credentials);
                let can_connect = picked
                    .as_ref()
                    .is_some_and(|_| !needs_key || !secret.is_empty());

                let hub_connect = self.hub.clone();
                let hub_settings = self.hub.clone();
                let pod = cx.entity();

                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .when(providers.is_empty(), |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.ink_muted)
                                .child("接続先を読み込んでいます…"),
                        )
                    })
                    .when(!provider_chips.is_empty(), |this| {
                        this.child(
                            div()
                                .flex()
                                .flex_row()
                                .flex_wrap()
                                .gap(px(6.))
                                .children(provider_chips),
                        )
                    })
                    .child(self.onboard_key.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .child(pill_button(
                                "onboard-open-connections",
                                "詳細設定を開く",
                                Some(Icon::Plug),
                                Tone::Ghost,
                                true,
                                theme,
                                move |_, cx| {
                                    hub_settings.update(cx, |hub, cx| {
                                        hub.open_settings(SettingsSection::Connections, cx);
                                    });
                                },
                            ))
                            .child(pill_button(
                                "onboard-connect-llm",
                                "接続して次へ",
                                Some(Icon::Check),
                                Tone::Primary,
                                can_connect,
                                theme,
                                move |_, cx| {
                                    if let Some(provider) = picked.clone() {
                                        hub_connect.update(cx, |hub, cx| {
                                            hub.quick_connect_llm(provider, secret.clone(), cx);
                                        });
                                        pod.update(cx, |this, cx| {
                                            this.onboard_key.update(cx, |d, cx| d.clear(cx));
                                            this.onboarding_step = Some(1);
                                            cx.notify();
                                        });
                                    }
                                },
                            )),
                    )
                    .into_any_element()
            }
            1 => {
                let hub_tts = self.hub.clone();
                let pod_next = cx.entity();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child(if has_tts {
                                "音声合成の接続が有効になっています。"
                            } else {
                                "VOICEVOX（127.0.0.1:50021）を使う場合は追加できます。"
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .when(!has_tts, |this| {
                                this.child(pill_button(
                                    "onboard-add-voicevox",
                                    "VOICEVOXを追加",
                                    Some(Icon::Volume),
                                    Tone::Soft,
                                    true,
                                    theme,
                                    move |_, cx| {
                                        hub_tts.update(cx, |hub, cx| {
                                            hub.add_voicevox_preset(3, None, cx);
                                        });
                                    },
                                ))
                            })
                            .child(pill_button(
                                "onboard-voice-next",
                                if companions_empty {
                                    "次へ"
                                } else {
                                    "対話を始める"
                                },
                                Some(Icon::Check),
                                Tone::Primary,
                                true,
                                theme,
                                move |_, cx| {
                                    pod_next.update(cx, |this, cx| {
                                        this.onboarding_step =
                                            if companions_empty { Some(2) } else { None };
                                        cx.notify();
                                    });
                                },
                            )),
                    )
                    .into_any_element()
            }
            _ => {
                let look = theme.field_look(false, false);
                self.onboard_name.update(cx, |d, _| d.apply_look(look));
                let name = self.onboard_name.read(cx).text().trim().to_owned();
                let hub_start = self.hub.clone();
                let pod_done = cx.entity();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .when(companions_empty, |this| {
                        this.child(self.onboard_name.clone())
                    })
                    .child(div().flex().flex_row().justify_end().child(pill_button(
                        "onboard-finish",
                        "対話を始める",
                        Some(Icon::Sparkles),
                        Tone::Primary,
                        !companions_empty || !name.is_empty(),
                        theme,
                        move |_, cx| {
                            if companions_empty {
                                let final_name = if name.is_empty() {
                                    "エネ".to_owned()
                                } else {
                                    name.clone()
                                };
                                hub_start.update(cx, |hub, cx| {
                                    hub.create_companion(
                                        NewCompanionSpec {
                                            name: final_name,
                                            persona: "柔らかく親しみやすい口調で会話します。"
                                                .into(),
                                            mind: "相手を思いやりながら率直に対話する。".into(),
                                            voice_identifier: None,
                                        },
                                        cx,
                                    );
                                });
                            }
                            pod_done.update(cx, |this, cx| {
                                this.onboarding_step = None;
                                cx.notify();
                            });
                        },
                    )))
                    .into_any_element()
            }
        };

        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_mini_body(
        &self,
        streaming_preview: &str,
        is_busy: bool,
        stop_requested: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hub = self.hub.read(cx);
        let latest_reply = hub
            .model
            .chat
            .messages
            .iter()
            .rev()
            .find(|m| m.role != w::MessageRole::User)
            .map(|m| m.text.clone());

        let speech_text = if !streaming_preview.is_empty() {
            streaming_preview.to_owned()
        } else if let Some(reply) = latest_reply {
            reply
        } else {
            "下の入力欄からメッセージを送るか、マイクから話しかけてみてください。".to_owned()
        };

        let transcription = hub.model.transcription.clone();
        let hub_stop = self.hub.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.ink)
                    .line_clamp(3)
                    .child(speech_text),
            )
            .when_some(transcription, |this, live_stt| {
                this.child(
                    div()
                        .rounded(px(12.))
                        .bg(theme.mint_soft)
                        .px(px(10.))
                        .py(px(5.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(icon(Icon::Mic, 12., theme.mint_fg))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.mint_fg)
                                .truncate()
                                .child(if live_stt.text.trim().is_empty() {
                                    "音声を認識中…".to_owned()
                                } else {
                                    format!("「{}」", live_stt.text)
                                }),
                        ),
                )
            })
            .when(is_busy, |this| {
                this.child(div().flex().flex_row().justify_end().child(pill_button(
                    "pod-stop-inline",
                    if stop_requested {
                        "停止中"
                    } else {
                        "応答を停止"
                    },
                    Some(Icon::Stop),
                    Tone::Danger,
                    !stop_requested,
                    theme,
                    move |_, cx| {
                        hub_stop.update(cx, |hub, cx| hub.stop(cx));
                    },
                )))
            })
            .into_any_element()
    }

    fn render_expanded_timeline(
        &self,
        streaming_preview: &str,
        is_busy: bool,
        stop_requested: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hub = self.hub.read(cx);
        let mut rows: Vec<AnyElement> = Vec::new();

        if hub.model.chat.loading && hub.model.chat.messages.is_empty() {
            rows.push(
                div()
                    .py(px(16.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("履歴を読み込んでいます…")
                    .into_any_element(),
            );
        }
        if let Some(err) = hub.model.chat.error.as_ref() {
            rows.push(
                div()
                    .rounded(px(12.))
                    .bg(theme.danger_soft)
                    .px(px(10.))
                    .py(px(8.))
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child(format!("履歴を取得できませんでした: {err}"))
                    .into_any_element(),
            );
        }

        for msg in &hub.model.chat.messages {
            let is_user = msg.role == w::MessageRole::User;
            rows.push(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .when(is_user, |this| this.items_end())
                    .when(!is_user, |this| this.items_start())
                    .child(
                        div()
                            .max_w(px(300.))
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(14.))
                            .when(is_user, |this| {
                                this.rounded_br(px(4.))
                                    .bg(theme.bg_user_msg)
                                    .text_color(theme.ink)
                            })
                            .when(!is_user, |this| {
                                this.rounded_bl(px(4.))
                                    .bg(theme.bg_blush)
                                    .text_color(theme.ink)
                            })
                            .text_sm()
                            .child(SharedString::from(msg.text.clone())),
                    )
                    .into_any_element(),
            );
        }

        if !streaming_preview.is_empty() {
            rows.push(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(
                        div()
                            .max_w(px(300.))
                            .px(px(12.))
                            .py(px(8.))
                            .rounded(px(14.))
                            .rounded_bl(px(4.))
                            .bg(theme.bg_blush)
                            .text_color(theme.ink)
                            .text_sm()
                            .child(SharedString::from(streaming_preview.to_owned())),
                    )
                    .into_any_element(),
            );
        }

        if rows.is_empty() && !hub.model.chat.loading {
            rows.push(
                div()
                    .py(px(20.))
                    .text_center()
                    .text_xs()
                    .text_color(theme.ink_placeholder)
                    .child("会話履歴はまだありません")
                    .into_any_element(),
            );
        }

        let transcription = hub.model.transcription.clone();
        let hub_stop = self.hub.clone();

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("pod-history-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .children(rows),
            )
            .when_some(transcription, |this, live_stt| {
                this.child(
                    div()
                        .rounded(px(12.))
                        .bg(theme.mint_soft)
                        .px(px(10.))
                        .py(px(5.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.))
                        .child(icon(Icon::Mic, 12., theme.mint_fg))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.mint_fg)
                                .truncate()
                                .child(format!("「{}」", live_stt.text)),
                        ),
                )
            })
            .when(is_busy, |this| {
                this.child(div().flex().flex_row().justify_end().child(pill_button(
                    "pod-stop-expanded",
                    if stop_requested {
                        "停止中"
                    } else {
                        "応答を停止"
                    },
                    Some(Icon::Stop),
                    Tone::Danger,
                    !stop_requested,
                    theme,
                    move |_, cx| {
                        hub_stop.update(cx, |hub, cx| hub.stop(cx));
                    },
                )))
            })
            .into_any_element()
    }

    fn render_capsule_dock(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let (theme, live, companion, mic_muted, mic_ready, expanded, unsent, link_dot) = {
            let hub = self.hub.read(cx);
            let theme = hub.theme();
            let link_dot = match &hub.model.link {
                Link::Live { .. } => theme.mint,
                Link::Connecting { .. } => theme.amber,
                _ => theme.danger,
            };
            (
                theme,
                hub.live(),
                hub.model.companion().cloned(),
                hub.model.mic_muted,
                matches!(hub.model.devices.input, Device::Ready(_)),
                hub.pod_mode == PodMode::ExpandedChat,
                hub.model.chat.unsent.clone(),
                link_dot,
            )
        };

        let look = theme.field_look(true, true);
        self.chat_input
            .update(cx, |draft, _| draft.apply_look(look));

        let draft_text = self.chat_input.read(cx).text().to_owned();
        let too_long = draft_text.chars().count() > MAX_INPUT;
        let can_send = live && companion.is_some() && !draft_text.trim().is_empty() && !too_long;

        let comp_name = companion
            .as_ref()
            .map(|c| c.name.as_str())
            .unwrap_or("エネ");
        let comp_id = companion.as_ref().map(|c| c.id.as_str()).unwrap_or("");

        let mut unsent_rows: Vec<AnyElement> = Vec::new();
        for (idx, item) in unsent.into_iter().enumerate() {
            let key = item.request_key;
            let text = item.text;
            let retry_text = text.clone();
            let hub_retry = self.hub.clone();
            unsent_rows.push(
                div()
                    .w_full()
                    .rounded_full()
                    .bg(theme.danger_soft)
                    .px(px(12.))
                    .py(px(5.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.danger_fg)
                            .truncate()
                            .child(format!("未送信: {text}")),
                    )
                    .child(pill_button(
                        SharedString::from(format!("pod-retry-{idx}")),
                        "再送",
                        Some(Icon::Refresh),
                        Tone::Primary,
                        live,
                        &theme,
                        move |_, cx| {
                            hub_retry.update(cx, |hub, cx| {
                                hub.retry_unsent(&key, &retry_text, cx);
                            });
                        },
                    ))
                    .into_any_element(),
            );
        }

        let hub_mic = self.hub.clone();
        let hub_settings = self.hub.clone();
        let pod_toggle = cx.entity();

        let capsule = div()
            .flex_1()
            .min_w_0()
            .h(px(44.))
            .rounded_full()
            .bg(theme.bg_surface)
            .border_2()
            .border_color(theme.border_float)
            .shadow(theme.float_shadow())
            .px(px(6.))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .id("pod-switcher-btn")
                    .flex_none()
                    .p(px(2.))
                    .rounded_full()
                    .cursor_pointer()
                    .hover(|this| this.bg(theme.bg_blush))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                            this.roster_open = !this.roster_open;
                            cx.notify();
                        }),
                    )
                    .child(companion_badge(
                        comp_name,
                        comp_id,
                        28.,
                        Some(link_dot),
                        &theme,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(4.))
                    .child(self.chat_input.clone()),
            )
            .child(
                div()
                    .id("pod-send-btn")
                    .flex_none()
                    .w(px(30.))
                    .h(px(30.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if can_send {
                        theme.accent
                    } else {
                        theme.bg_blush
                    })
                    .when(can_send, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(theme.accent_hover))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                                    this.submit(cx);
                                }),
                            )
                    })
                    .child(icon(
                        Icon::Send,
                        14.,
                        if can_send {
                            theme.accent_fg
                        } else {
                            theme.ink_placeholder
                        },
                    )),
            )
            .child(
                div()
                    .id("pod-mic-btn")
                    .flex_none()
                    .w(px(30.))
                    .h(px(30.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(if mic_muted {
                        theme.bg_blush
                    } else {
                        theme.mint_soft
                    })
                    .cursor_pointer()
                    .hover(|this| {
                        this.bg(if mic_muted {
                            theme.bg_milk
                        } else {
                            theme.mint_soft.alpha(0.84)
                        })
                    })
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                        hub_mic.update(cx, |hub, cx| hub.toggle_mic_mute(cx));
                    })
                    .child(icon(
                        if mic_muted { Icon::MicOff } else { Icon::Mic },
                        14.,
                        if mic_muted {
                            theme.ink_muted
                        } else if mic_ready && live {
                            theme.mint_fg
                        } else {
                            theme.ink_muted
                        },
                    )),
            );

        let orbs = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(floating_orb_button(
                "pod-orb-history",
                if expanded {
                    Icon::ChatCollapse
                } else {
                    Icon::ChatBubble
                },
                expanded,
                false,
                &theme,
                move |window, cx| {
                    pod_toggle.update(cx, |this, cx| this.toggle_mode(window, cx));
                },
            ))
            .child(floating_orb_button(
                "pod-orb-settings",
                Icon::Settings,
                false,
                false,
                &theme,
                move |_, cx| {
                    hub_settings.update(cx, |hub, cx| {
                        hub.open_settings(SettingsSection::Companions, cx);
                    });
                },
            ))
            .child(floating_orb_button(
                "pod-orb-close",
                Icon::Close,
                false,
                true,
                &theme,
                |_, cx| {
                    cx.quit();
                },
            ));

        div()
            .w_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .children(unsent_rows)
            .when(too_long, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.danger_fg)
                        .px(px(8.))
                        .child(format!("入力文字数が上限（{MAX_INPUT}文字）を超えています")),
                )
            })
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(capsule)
                    .child(orbs),
            )
            .into_any_element()
    }

    fn render_roster_popover(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let companions = hub
            .model
            .snapshot()
            .map(|s| s.companions.clone())
            .unwrap_or_default();
        let active_id = hub.model.companion_id().map(str::to_owned);

        let mut rows: Vec<AnyElement> = Vec::new();
        for comp in companions {
            let cid = comp.id.clone();
            let active = active_id.as_deref() == Some(cid.as_str());
            let persona = comp
                .persona
                .as_deref()
                .map(|s| s.trim().replace('\n', " "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "話し方未設定".to_owned());
            let pick_id = cid.clone();
            rows.push(
                div()
                    .id(SharedString::from(format!("pod-roster-{}", comp.id)))
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(12.))
                    .bg(if active {
                        theme.bg_blush
                    } else {
                        theme.bg_surface
                    })
                    .cursor_pointer()
                    .hover(|this| this.bg(theme.bg_blush))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                            this.hub
                                .update(cx, |hub, cx| hub.select_companion(&pick_id, cx));
                            this.roster_open = false;
                            this.focus_input(window, cx);
                            cx.notify();
                        }),
                    )
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(companion_badge(
                        &comp.name,
                        &cid,
                        28.,
                        active.then_some(theme.mint),
                        theme,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .text_color(theme.ink)
                                    .truncate()
                                    .child(comp.name),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(persona),
                            ),
                    )
                    .when(active, |this| {
                        this.child(icon(Icon::Check, 14., theme.accent))
                    })
                    .into_any_element(),
            );
        }

        if rows.is_empty() {
            rows.push(
                div()
                    .px(px(10.))
                    .py(px(10.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("コンパニオンがいません")
                    .into_any_element(),
            );
        }

        let hub_manage = self.hub.clone();
        let pod_close = cx.entity();

        div()
            .id("pod-roster-popover")
            .absolute()
            .bottom(px(68.))
            .left(px(14.))
            .w(px(264.))
            .bg(theme.bg_surface)
            .border_2()
            .border_color(theme.border_float)
            .shadow(theme.float_shadow())
            .rounded(px(18.))
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .id("pod-roster-scroll")
                    .max_h(px(230.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .children(rows),
            )
            .child(
                div()
                    .pt(px(4.))
                    .border_t_1()
                    .border_color(theme.border)
                    .flex()
                    .flex_row()
                    .justify_end()
                    .child(pill_button(
                        "pod-roster-manage",
                        "コンパニオン管理",
                        Some(Icon::Settings),
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            hub_manage.update(cx, |hub, cx| {
                                hub.open_settings(SettingsSection::Companions, cx);
                            });
                            pod_close.update(cx, |this, cx| {
                                this.roster_open = false;
                                cx.notify();
                            });
                        },
                    )),
            )
            .into_any_element()
    }
}

impl Render for CompanionPod {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.title_initialized {
            self.title_initialized = true;
            window.set_window_title("ene-companion-pod");
            #[cfg(windows)]
            if let Some(hwnd) = current_pod_hwnd() {
                suppress_dwm_frame(hwnd);
            }
        }
        let desired = self.desired_size(cx);
        let (scroll_ping, theme) = {
            let hub = self.hub.read(cx);
            (hub.pod_scroll_ping, hub.theme())
        };
        self.apply_size(desired, window, cx);
        if scroll_ping != self.last_scroll_ping {
            self.last_scroll_ping = scroll_ping;
            self.scroll.scroll_to_bottom();
        }

        let roster_open = self.roster_open;

        div()
            .relative()
            .size_full()
            .bg(gpui::transparent_black())
            .font({
                let mut f = gpui::font(resolve_ui_font_family(cx));
                f.fallbacks = Some(ui_font_fallbacks());
                f
            })
            .text_color(theme.ink)
            .p(px(14.))
            .flex()
            .flex_col()
            .justify_end()
            .gap(px(10.))
            .child(self.render_bubble_stage(cx))
            .child(self.render_capsule_dock(cx))
            .when(roster_open, |this| {
                this.child(self.render_roster_popover(&theme, cx))
            })
    }
}

/// Binds keyboard shortcuts (`Enter` send in `ChatInput`, `Esc` stop/collapse) globally.
pub fn bind(cx: &mut App, view: WeakEntity<CompanionPod>) {
    input::bind(cx);
    cx.bind_keys([
        KeyBinding::new("enter", Submit, Some("ChatInput")),
        KeyBinding::new("escape", CancelOrStop, None),
    ]);

    let submit_view = view.clone();
    cx.on_action(move |_: &Submit, cx: &mut App| {
        let _ = submit_view.update(cx, |this, cx| this.submit(cx));
    });

    cx.on_action(move |_: &CancelOrStop, cx: &mut App| {
        let _ = view.update(cx, |this, cx| {
            if this.roster_open {
                this.roster_open = false;
                cx.notify();
                return;
            }
            let hub = this.hub.read(cx);
            let busy = hub.model.companion_speaking || !hub.model.streaming.is_empty();
            let expanded = hub.pod_mode == PodMode::ExpandedChat;
            if busy {
                this.hub.update(cx, |hub, cx| hub.stop(cx));
            } else if expanded {
                this.hub
                    .update(cx, |hub, cx| hub.set_pod_mode(PodMode::MiniVoice, cx));
            }
        });
    });
}

/// Focuses the pod capsule input field when the window opens.
pub fn focus_chat(pod: &CompanionPod, window: &mut Window, cx: &mut App) {
    pod.focus_input(window, cx);
}

#[cfg(windows)]
fn current_pod_hwnd() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::{
        Foundation::{HWND, LPARAM},
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::{EnumThreadWindows, GetWindowTextW},
    };
    unsafe extern "system" fn find_pod(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let mut buf = [0u16; 64];
        let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
        if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "ene-companion-pod" {
            let slot = lparam.0 as *mut Option<HWND>;
            unsafe { *slot = Some(hwnd) };
            return false.into();
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    let lparam = LPARAM(&raw mut found as isize);
    unsafe {
        let _ = EnumThreadWindows(GetCurrentThreadId(), Some(find_pod), lparam);
    }
    found
}

#[cfg(windows)]
fn read_window_bottom(hwnd: windows::Win32::Foundation::HWND) -> Option<i32> {
    use windows::Win32::{Foundation::RECT, UI::WindowsAndMessaging::GetWindowRect};
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect.bottom)
}

#[cfg(windows)]
fn anchor_window_bottom(hwnd_raw: isize, target_bottom: i32) {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        UI::WindowsAndMessaging::{
            GetWindowRect, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
        },
    };
    let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return;
    }
    let height = rect.bottom - rect.top;
    let new_top = (target_bottom - height).max(16);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.left,
            new_top,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

#[cfg(windows)]
fn suppress_dwm_frame(hwnd: windows::Win32::Foundation::HWND) {
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: *mut std::ffi::c_void,
            dw_attribute: u32,
            pv_attribute: *const std::ffi::c_void,
            cb_attribute: u32,
        ) -> i32;
    }
    const DWMWA_NCRENDERING_POLICY: u32 = 2;
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWA_BORDER_COLOR: u32 = 34;
    let ncrp: u32 = 1; // DWMNCRP_DISABLED
    let corner: u32 = 1; // DWMWCP_DONOTROUND
    let color: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_NCRENDERING_POLICY,
            (&raw const ncrp).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const corner).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(hwnd.0, DWMWA_BORDER_COLOR, (&raw const color).cast(), 4);
    }
}
