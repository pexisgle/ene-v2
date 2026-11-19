//! Minimal frameless floating companion window: speech bubble, inline approval,
//! first-run onboarding, collapsible history, capsule composer and controls menu.

use super::icons::{Icon, icon};
use super::theme::{Theme, resolve_ui_font_family, ui_font_fallbacks};
use super::widgets::{
    DropdownOption, Tone, companion_badge, dropdown_select, floating_orb_button, pill_button,
    status_pill, window_drag_area,
};
use super::{Hub, NewCompanionSpec, PodMode, SettingsSection};
use crate::{
    input::{self, Draft},
    state::{Device, Link, NoticeKind},
};
use ene_rpc as w;
use gpui::{
    AnyElement, App, Bounds, Context, Entity, KeyBinding, MouseButton, MouseDownEvent, Pixels,
    ScrollHandle, SharedString, Size, Subscription, Window, actions, div, point, prelude::*, px,
};

const MAX_INPUT: usize = 4000;

actions!(ene_desktop, [Submit, CancelOrStop]);

#[derive(Clone, Copy)]
enum ChatControl {
    Companions,
    Microphone { ready: bool },
    CaptureMode,
    PttToggle,
    PttCancel,
    Speaker { ready: bool },
    History,
    Settings,
    Close,
}

/// Compatibility alias expected by `lib.rs`.
pub type Desktop = CompanionPod;

/// Computes default bottom-right floating placement on the primary display.
pub fn initial_pod_bounds(cx: &App) -> Bounds<Pixels> {
    initial_pod_bounds_for_mode(PodMode::MiniVoice, cx)
}

/// Computes bottom-right placement sized for the requested pod mode.
pub fn initial_pod_bounds_for_mode(mode: PodMode, cx: &App) -> Bounds<Pixels> {
    let size = mode.size();
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
    setup_model_id: Option<String>,
    setup_model_dropdown_open: bool,
    setup_model_filter: Entity<Draft>,
    setup_voice_speaker_id: Option<String>,
    setup_voice_dropdown_open: bool,
    setup_voice_filter: Entity<Draft>,
    selected_provider_id: Option<String>,
    choosing_provider: bool,
    key_focused: bool,
    roster_open: bool,
    controls_open: bool,
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
            draft.set("ene", cx);
            draft
        });
        let setup_model_filter = cx.new(|cx| Draft::new(cx, "モデルを検索…", "TextField"));
        let setup_voice_filter = cx.new(|cx| Draft::new(cx, "話者を検索…", "TextField"));
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            chat_input,
            onboard_key,
            onboard_name,
            setup_model_id: None,
            setup_model_dropdown_open: false,
            setup_model_filter,
            setup_voice_speaker_id: None,
            setup_voice_dropdown_open: false,
            setup_voice_filter,
            selected_provider_id: None,
            choosing_provider: false,
            key_focused: false,
            roster_open: false,
            controls_open: false,
            last_size: PodMode::MiniVoice.size(),
            title_initialized: false,
            scroll: ScrollHandle::new(),
            last_scroll_ping: 0,
            _observe,
        }
    }

    fn needs_connection(&self, cx: &App) -> bool {
        let hub = self.hub.read(cx);
        hub.live()
            && hub.model.settings().is_some_and(|settings| {
                hub.setup_pending
                    || hub.setup_error.is_some()
                    || hub.model.providers.as_deref().is_none_or(|providers| {
                        !settings
                            .connections
                            .iter()
                            .any(|c| super::can_start_chat(c, providers))
                    })
            })
    }

    fn in_model_setup(&self, cx: &App) -> bool {
        let hub = self.hub.read(cx);
        if !hub.live() {
            return false;
        }
        let companions_empty = hub
            .model
            .snapshot()
            .is_none_or(|snap| snap.companions.is_empty());
        hub.pending_setup_connection_id.is_some() || companions_empty
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        let field = if self.needs_connection(cx) {
            &self.onboard_key
        } else if self.in_model_setup(cx) {
            let companions_empty = self
                .hub
                .read(cx)
                .model
                .snapshot()
                .is_none_or(|snap| snap.companions.is_empty());
            if companions_empty {
                &self.onboard_name
            } else {
                &self.setup_model_filter
            }
        } else {
            &self.chat_input
        };
        let handle = field.read(cx).focus_handle();
        window.focus(&handle);
    }

    pub(crate) fn sync_mode_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let desired = self.desired_size(cx);
        self.apply_size(desired, window, cx);
        self.focus_input(window, cx);
        ensure_pod_topmost();
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
        let needs_conn = self.needs_connection(cx);
        let model_setup = self.in_model_setup(cx);
        if needs_conn || model_setup {
            let base = if needs_conn {
                if hub.setup_error_detail.is_some() {
                    452.
                } else {
                    432.
                }
            } else if self.setup_model_dropdown_open || self.setup_voice_dropdown_open {
                580.
            } else {
                492.
            };
            let h = if hub.model.notices.is_empty() {
                base
            } else {
                base + 44.
            };
            gpui::size(px(444.), px(h))
        } else if !hub.model.approvals().is_empty() || self.roster_open {
            gpui::size(px(444.), px(376.))
        } else {
            let speech_text = active_mini_speech_text(&hub.model);
            let mut estimated_lines = 0usize;
            for line in speech_text.lines() {
                let chars = line.chars().count();
                estimated_lines += chars.max(1).div_ceil(24);
            }
            estimated_lines = estimated_lines.clamp(2, 7);
            let mut h = 152.0 + (estimated_lines as f32) * 20.0;
            if estimated_lines > 6 || hub.model.chat.messages.is_empty() {
                h += 40.0;
            }
            if !extract_urls(&speech_text).is_empty() {
                h += 28.0;
            }
            if hub.model.transcription.is_some() {
                h += 38.0;
            }
            if !hub.model.notices.is_empty() {
                h += 46.0;
            }
            let turn_failed = hub
                .model
                .chat
                .turn
                .as_ref()
                .and_then(|t| t.status.as_known())
                == Some(w::TurnStatus::Failed);
            if hub.model.companion_speaking
                || !hub.model.streaming.is_empty()
                || hub.model.chat.stop_requested
                || turn_failed
            {
                h += 36.0;
            }
            gpui::size(px(444.), px(h.clamp(188., 400.)))
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
        if self.needs_connection(cx)
            || self.in_model_setup(cx)
            || self.chat_input.read(cx).composing()
        {
            return;
        }
        let text = self.chat_input.read(cx).text().trim().to_owned();
        if text.is_empty() || text.chars().count() > MAX_INPUT {
            return;
        }
        let sent = self.hub.update(cx, |hub, cx| hub.send(text, cx));
        if sent {
            self.controls_open = false;
            self.chat_input.update(cx, |draft, cx| draft.clear(cx));
        }
    }

    fn render_bubble_stage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let theme = hub.theme();
        let live = hub.live();
        let expanded = hub.pod_mode == PodMode::ExpandedChat;
        let companion = hub.model.companion().cloned();
        let approvals = hub.model.approvals().to_vec();
        let notices = hub.model.notices.clone();
        let companion_name = companion
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "ene".to_owned());

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

        let in_onboarding = self.needs_connection(cx) || self.in_model_setup(cx);

        let status_word = match &hub.model.link {
            Link::Connecting { .. } => "接続中",
            Link::Lost { .. } | Link::Unset { .. } | Link::Unusable { .. } => "未接続",
            Link::Live { .. } if !approvals.is_empty() => "確認待ち",
            Link::Live { .. } if in_onboarding => "初期設定",
            Link::Live { .. } if hub.model.work_active => "作業中",
            Link::Live { .. } if speaking => "発話中",
            Link::Live { .. } if !hub.model.chat.sending.is_empty() => "送信中",
            Link::Live { .. }
                if !streaming_preview.is_empty()
                    || hub
                        .model
                        .chat
                        .turn
                        .as_ref()
                        .is_some_and(|t| t.status == w::TurnStatus::Streaming) =>
            {
                "返答を考え中"
            }
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
            self.render_onboarding_body(self.needs_connection(cx), &theme, cx)
        } else if expanded {
            self.render_expanded_timeline(&streaming_preview, is_busy, stop_requested, &theme, cx)
        } else {
            self.render_mini_body(&streaming_preview, is_busy, stop_requested, &theme, cx)
        };

        let drag_strip = window_drag_area(
            div()
                .w_full()
                .h(if expanded { px(18.) } else { px(10.) })
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

        let badge = window_drag_area(
            div()
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
                .child(badge_label),
            false,
        );

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
                        .id("approval-arguments")
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .child(args.clone()),
                )
            })
            .child(div().text_xs().child(if args.is_empty() {
                "常に許可すると、次回からこの操作を引数にかかわらず許可します。"
            } else {
                "常に許可すると、次回からこの操作を同じ引数で許可します。"
            }))
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
        needs_connection: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if needs_connection {
            let providers = self
                .hub
                .read(cx)
                .model
                .providers
                .clone()
                .unwrap_or_default();
            if providers.is_empty() {
                let hub = self.hub.clone();
                return div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child("接続先を読み込んでいます…")
                    .child(pill_button(
                        "setup-settings",
                        "接続設定を開く",
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            hub.update(cx, |hub, cx| {
                                hub.open_settings(SettingsSection::Connections, cx)
                            })
                        },
                    ))
                    .into_any_element();
            }
            let picked = self
                .selected_provider_id
                .as_deref()
                .and_then(|id| providers.iter().find(|p| p.id == id))
                .or_else(|| providers.iter().find(|p| p.id == "openrouter"))
                .or_else(|| providers.first())
                .cloned();

            let mut provider_chips = Vec::new();
            for p in &providers {
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
                            if !active {
                                this.onboard_key.update(cx, |d, cx| d.clear(cx));
                            }
                            this.choosing_provider = false;
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
            let selector = cx.entity();
            let hub = self.hub.read(cx);
            let pending = hub.setup_pending;
            let name = picked
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("AIサービス");
            let is_agent_platform = picked
                .as_ref()
                .is_some_and(|p| p.adapter == super::GOOGLE_AGENT_PLATFORM);
            let keys_url = picked.as_ref().and_then(|p| match p.id.as_str() {
                "openrouter" => Some("https://openrouter.ai/settings/keys"),
                "openai" => Some("https://platform.openai.com/api-keys"),
                "anthropic" => Some("https://console.anthropic.com/settings/keys"),
                "google" | "gemini" => Some("https://aistudio.google.com/apikey"),
                super::GOOGLE_AGENT_PLATFORM => {
                    Some("https://console.cloud.google.com/agent-platform/api-keys")
                }
                "sakura" => Some("https://secure.sakura.ad.jp/cloud/"),
                _ => None,
            });
            return div()
                .flex()
                .flex_col()
                .gap(px(8.))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child("eneと話す準備"),
                )
                .child(div().text_sm().child(if is_agent_platform {
                    "Express mode の API キーを入力してください。Cloud プロジェクトを指定する場合は接続設定で登録できます。".into()
                } else if needs_key {
                    format!("会話は{name}に送られます。{name}の設定で発行したAPIキーを貼り付けてください。")
                } else {
                    format!("会話は{name}に送られます。接続して会話を始めてください。")
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(6.))
                        .when_some(keys_url, |row, url| {
                            row.child(pill_button(
                                "setup-keys",
                                "キーの取得先を開く",
                                None,
                                Tone::Soft,
                                !pending,
                                theme,
                                move |_, cx| cx.open_url(url),
                            ))
                        })
                        .when(is_agent_platform, |row| {
                            let hub = hub_settings.clone();
                            row.child(pill_button(
                                "setup-agent-platform-settings",
                                "接続設定を開く",
                                None,
                                Tone::Ghost,
                                !pending,
                                theme,
                                move |_, cx| {
                                    hub.update(cx, |hub, cx| {
                                        hub.open_settings(SettingsSection::Connections, cx)
                                    })
                                },
                            ))
                        })
                        .child(pill_button(
                            "setup-choose",
                            "接続先を変更",
                            None,
                            Tone::Ghost,
                            !pending,
                            theme,
                            move |_, cx| {
                                selector.update(cx, |this, cx| {
                                    this.choosing_provider = !this.choosing_provider;
                                    cx.notify();
                                })
                            },
                        )),
                )
                .when(self.choosing_provider && !pending, |this| {
                    this.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.))
                            .children(provider_chips)
                            .child(pill_button(
                                "setup-local",
                                "自分の接続先を設定",
                                None,
                                Tone::Ghost,
                                true,
                                theme,
                                move |_, cx| {
                                    hub_settings.update(cx, |hub, cx| {
                                        hub.open_settings(SettingsSection::Connections, cx)
                                    })
                                },
                            )),
                    )
                })
                .child(self.onboard_key.clone())
                .when_some(hub.setup_error, |row, err| {
                    row.child(div().text_xs().text_color(theme.danger_fg).child(err))
                })
                .when_some(hub.setup_error_detail.clone(), |row, detail| {
                    row.child(div().text_xs().text_color(theme.danger_fg).child(detail))
                })
                .child(div().flex().justify_end().child(pill_button(
                    "onboard-connect-llm",
                    if pending {
                        "接続してモデルを取得中…".to_owned()
                    } else {
                        format!("{name}に接続してモデルを取得")
                    },
                    Some(Icon::Check),
                    Tone::Primary,
                    can_connect && !pending,
                    theme,
                    move |_, cx| {
                        if let Some(provider) = picked.clone() {
                            hub_connect.update(cx, |hub, cx| {
                                hub.quick_connect_llm(provider, secret.clone(), cx)
                            });
                            pod.update(cx, |this, cx| {
                                this.onboard_key.update(cx, |d, cx| d.clear(cx));
                                cx.notify();
                            });
                        }
                    },
                )))
                .into_any_element();
        }
        let look = theme.field_look(false, false);
        self.onboard_name.update(cx, |d, _| d.apply_look(look));
        self.setup_model_filter
            .update(cx, |d, _| d.apply_look(look));
        self.setup_voice_filter
            .update(cx, |d, _| d.apply_look(look));
        let companions_empty = self
            .hub
            .read(cx)
            .model
            .snapshot()
            .is_none_or(|snap| snap.companions.is_empty());
        let (
            target_conn_id,
            model_options,
            selected_model,
            selected_model_label,
            checking_models,
            voicevox_conn_id,
            voicevox_enabled,
            voicevox_checking,
            voice_options,
            selected_voice,
            selected_voice_label,
        ) = {
            let hub = self.hub.read(cx);
            let target_conn_id = hub.pending_setup_connection_id.clone().or_else(|| {
                let providers = hub.model.providers.as_deref()?;
                hub.model.settings().and_then(|settings| {
                    settings
                        .connections
                        .iter()
                        .find(|c| super::can_start_chat(c, providers))
                        .map(|c| c.id.clone())
                })
            });
            let mut model_pairs = target_conn_id
                .as_deref()
                .map(|cid| hub.available_models_for_connection(cid))
                .unwrap_or_default();
            for model_id in &hub.pending_setup_models {
                if !model_pairs.iter().any(|(id, _)| id == model_id) {
                    model_pairs.push((model_id.clone(), model_id.clone()));
                }
            }
            let model_options: Vec<DropdownOption> = model_pairs
                .iter()
                .map(|(id, label)| DropdownOption {
                    value: id.clone(),
                    label: label.clone(),
                    detail: (label != id).then(|| id.clone()),
                })
                .collect();
            let selected_model = self
                .setup_model_id
                .as_deref()
                .filter(|id| model_options.iter().any(|opt| opt.value == *id))
                .or_else(|| model_options.first().map(|opt| opt.value.as_str()))
                .map(str::to_owned);
            let selected_model_label = selected_model.as_deref().and_then(|id| {
                model_options
                    .iter()
                    .find(|opt| opt.value == id)
                    .map(|opt| opt.label.clone())
            });
            let checking_models = target_conn_id
                .as_deref()
                .is_some_and(|cid| hub.checking_connections.contains(cid));

            let voicevox_conn = hub.model.settings().and_then(|settings| {
                settings.connections.iter().find(|c| {
                    c.kind == w::ConnectionKind::Tts
                        && (c.id == "voicevox"
                            || matches!(
                                c.config.as_option().and_then(|cfg| cfg.config.as_ref()),
                                Some(w::connection_config::Config::Tts(tts))
                                    if tts
                                        .endpoint
                                        .as_option()
                                        .is_some_and(|ep| ep.adapter == "voicevox")
                            ))
                })
            });
            let voicevox_conn_id = voicevox_conn.map(|c| c.id.clone());
            let voicevox_enabled = voicevox_conn.is_some_and(|c| c.enabled);
            let voicevox_checking = voicevox_conn_id
                .as_deref()
                .is_some_and(|cid| hub.checking_connections.contains(cid))
                || hub.checking_connections.contains("voicevox");
            let voice_pairs = voicevox_conn_id
                .as_deref()
                .map(|cid| hub.voice_choices_for_connection(cid))
                .unwrap_or_default();
            let voice_options: Vec<DropdownOption> = voice_pairs
                .iter()
                .map(|(vid, label)| DropdownOption {
                    value: vid.clone(),
                    label: label.clone(),
                    detail: (label != vid).then(|| format!("話者ID: {vid}")),
                })
                .collect();
            let selected_voice = self
                .setup_voice_speaker_id
                .as_deref()
                .filter(|id| voice_options.iter().any(|opt| opt.value == *id))
                .or_else(|| voice_options.first().map(|opt| opt.value.as_str()))
                .map(str::to_owned);
            let selected_voice_label = selected_voice.as_deref().and_then(|id| {
                voice_options
                    .iter()
                    .find(|opt| opt.value == id)
                    .map(|opt| opt.label.clone())
            });
            (
                target_conn_id,
                model_options,
                selected_model,
                selected_model_label,
                checking_models,
                voicevox_conn_id,
                voicevox_enabled,
                voicevox_checking,
                voice_options,
                selected_voice,
                selected_voice_label,
            )
        };

        let name = self.onboard_name.read(cx).text().trim().to_owned();
        let model_filter_text = self.setup_model_filter.read(cx).text().to_owned();
        let voice_filter_text = self.setup_voice_filter.read(cx).text().to_owned();
        let pod_model_toggle = cx.entity();
        let pod_model_select = cx.entity();
        let pod_voice_toggle = cx.entity();
        let pod_voice_select = cx.entity();
        let hub_refresh_models = self.hub.clone();
        let hub_voicevox = self.hub.clone();
        let hub_start = self.hub.clone();
        let conn_for_refresh = target_conn_id.clone();
        let conn_for_start = target_conn_id.clone();
        let model_for_start = selected_model.clone();
        let voice_for_start = selected_voice.clone();
        let vv_id_for_btn = voicevox_conn_id.clone();

        let can_finish = (!companions_empty || !name.is_empty())
            && ((conn_for_start.is_some() && model_for_start.is_some())
                || (companions_empty && model_for_start.is_none() && !checking_models));

        let mut body = div().flex().flex_col().gap(px(8.)).child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child("使うモデルと音声を選択"),
        );

        if let Some(refresh_cid) = conn_for_refresh {
            body = body
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.ink_muted)
                                .child("使用モデル"),
                        )
                        .child(pill_button(
                            "onboard-refresh-models",
                            if checking_models {
                                "取得中…".to_owned()
                            } else {
                                "再取得".to_owned()
                            },
                            None,
                            Tone::Ghost,
                            !checking_models,
                            theme,
                            move |_, cx| {
                                hub_refresh_models.update(cx, |hub, cx| {
                                    hub.check_connection(refresh_cid.clone(), cx)
                                });
                            },
                        )),
                )
                .child(dropdown_select(
                    "onboard-model-dropdown",
                    selected_model.as_deref(),
                    selected_model_label.as_deref(),
                    if checking_models {
                        "モデル一覧を取得中…"
                    } else {
                        "モデルを選択…"
                    },
                    &model_options,
                    self.setup_model_dropdown_open,
                    !checking_models,
                    168.,
                    Some(self.setup_model_filter.clone()),
                    &model_filter_text,
                    "取得済みのモデルがありません（「再取得」で確認）",
                    theme,
                    move |_, cx| {
                        pod_model_toggle.update(cx, |this, cx| {
                            this.setup_model_dropdown_open = !this.setup_model_dropdown_open;
                            if this.setup_model_dropdown_open {
                                this.setup_voice_dropdown_open = false;
                            }
                            cx.notify();
                        });
                    },
                    move |val, _, cx| {
                        pod_model_select.update(cx, |this, cx| {
                            this.setup_model_id = Some(val.clone());
                            this.setup_model_dropdown_open = false;
                            this.setup_model_filter.update(cx, |d, cx| d.clear(cx));
                            cx.notify();
                        });
                    },
                ));
        }

        if voicevox_enabled && !voice_options.is_empty() {
            let vv_refresh = hub_voicevox.clone();
            let vv_cid = vv_id_for_btn.clone();
            body = body
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.ink_muted)
                                .child("VOICEVOXの話者"),
                        )
                        .when_some(vv_cid, |row, cid| {
                            row.child(pill_button(
                                "onboard-refresh-voicevox",
                                if voicevox_checking {
                                    "起動・取得中…".to_owned()
                                } else {
                                    "再取得".to_owned()
                                },
                                None,
                                Tone::Ghost,
                                !voicevox_checking,
                                theme,
                                move |_, cx| {
                                    vv_refresh.update(cx, |hub, cx| {
                                        hub.check_connection(cid.clone(), cx)
                                    });
                                },
                            ))
                        }),
                )
                .child(dropdown_select(
                    "onboard-voice-dropdown",
                    selected_voice.as_deref(),
                    selected_voice_label.as_deref(),
                    "話者を選択…",
                    &voice_options,
                    self.setup_voice_dropdown_open,
                    !voicevox_checking,
                    148.,
                    Some(self.setup_voice_filter.clone()),
                    &voice_filter_text,
                    "話者が見つかりません",
                    theme,
                    move |_, cx| {
                        pod_voice_toggle.update(cx, |this, cx| {
                            this.setup_voice_dropdown_open = !this.setup_voice_dropdown_open;
                            if this.setup_voice_dropdown_open {
                                this.setup_model_dropdown_open = false;
                            }
                            cx.notify();
                        });
                    },
                    move |val, _, cx| {
                        pod_voice_select.update(cx, |this, cx| {
                            this.setup_voice_speaker_id = Some(val.clone());
                            this.setup_voice_dropdown_open = false;
                            this.setup_voice_filter.update(cx, |d, cx| d.clear(cx));
                            cx.notify();
                        });
                    },
                ));
        } else {
            let vv_cid = vv_id_for_btn.clone();
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("VOICEVOX音声（任意）"),
                    )
                    .child(pill_button(
                        "onboard-launch-voicevox",
                        if voicevox_checking {
                            "VOICEVOXを起動中…".to_owned()
                        } else {
                            "VOICEVOXを起動して使う".to_owned()
                        },
                        Some(Icon::Volume),
                        Tone::Soft,
                        !voicevox_checking,
                        theme,
                        move |_, cx| {
                            let cid = vv_cid.clone();
                            hub_voicevox.update(cx, |hub, cx| {
                                if voicevox_enabled {
                                    if let Some(id) = cid {
                                        hub.check_connection(id, cx);
                                    }
                                } else {
                                    hub.add_voicevox_preset(3, None, cx);
                                }
                            });
                        },
                    )),
            );
        }

        if companions_empty {
            body = body
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .child("コンパニオンの名前"),
                )
                .child(self.onboard_name.clone());
        }

        body.child(div().flex().flex_row().justify_end().child(pill_button(
            "onboard-finish",
            if model_for_start.is_some() {
                "このモデルで始める"
            } else {
                "対話を始める"
            },
            Some(Icon::Sparkles),
            Tone::Primary,
            can_finish,
            theme,
            move |_, cx| {
                let name_arg = companions_empty.then(|| name.clone());
                let voice_arg = voice_for_start.clone();
                if let (Some(cid), Some(mid)) = (conn_for_start.clone(), model_for_start.clone()) {
                    hub_start.update(cx, |hub, cx| {
                        hub.complete_setup_selection(cid, mid, name_arg, voice_arg, cx);
                    });
                } else if companions_empty && !name.is_empty() {
                    hub_start.update(cx, |hub, cx| {
                        hub.create_companion(
                            NewCompanionSpec {
                                name: name.clone(),
                                persona: "柔らかく親しみやすい口調で会話します。".into(),
                                mind: "相手を思いやりながら率直に対話する。".into(),
                                voice_identifier: voice_arg,
                            },
                            cx,
                        );
                    });
                }
            },
        )))
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
        let speech_text = if !streaming_preview.is_empty() {
            streaming_preview.to_owned()
        } else {
            active_mini_speech_text(&hub.model)
        };
        let display_text = if streaming_preview.is_empty() {
            speech_text.clone()
        } else {
            preview_text(streaming_preview)
        };
        let long_reply = display_text
            .lines()
            .map(|line| line.chars().count().max(1).div_ceil(24))
            .sum::<usize>()
            > 6;
        let url_pills = render_url_pills(&speech_text, "mini", theme);
        let failed_turn = hub
            .model
            .chat
            .turn
            .as_ref()
            .filter(|turn| turn.status.as_known() == Some(w::TurnStatus::Failed))
            .map(|turn| {
                turn.failure
                    .as_option()
                    .map(|f| f.message.clone())
                    .filter(|msg| !msg.trim().is_empty())
                    .unwrap_or_else(|| "応答の生成に失敗しました".to_owned())
            });

        let transcription = hub.model.transcription.clone();
        let hub_stop = self.hub.clone();

        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("pod-mini-reply-scroll")
                    .max_h(px(144.))
                    .overflow_y_scroll()
                    .text_sm()
                    .text_color(theme.ink)
                    .child(display_text),
            )
            .when(long_reply, |row| {
                let more = self.hub.clone();
                row.child(pill_button(
                    "pod-more",
                    "全文を読む",
                    None,
                    Tone::Soft,
                    true,
                    theme,
                    move |_, cx| {
                        more.update(cx, |hub, cx| hub.set_pod_mode(PodMode::ExpandedChat, cx))
                    },
                ))
            })
            .when(
                hub.model.chat.messages.is_empty()
                    && !hub.model.chat.loading
                    && hub.model.chat.sending.is_empty()
                    && !is_busy,
                |body| {
                    let greeting = self.hub.clone();
                    body.child(pill_button(
                        "pod-greet",
                        "「こんにちは」を送る",
                        Some(Icon::Send),
                        Tone::Primary,
                        hub.model
                            .companion()
                            .and_then(|c| c.assignments.as_option())
                            .and_then(|a| a.companion.as_option())
                            .is_some(),
                        theme,
                        move |_, cx| {
                            greeting.update(cx, |hub, cx| hub.send("こんにちは".into(), cx));
                        },
                    ))
                },
            )
            .when_some(url_pills, |this, pills| this.child(pills))
            .when_some(failed_turn, |this, reason| {
                this.child(
                    div()
                        .rounded(px(10.))
                        .bg(theme.danger_soft)
                        .px(px(10.))
                        .py(px(5.))
                        .text_xs()
                        .text_color(theme.danger_fg)
                        .child(format!("エラー: {reason}")),
                )
            })
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

        for (msg_idx, msg) in hub.model.chat.messages.iter().enumerate() {
            let is_user = msg.role == w::MessageRole::User;
            let body_text = msg.text.clone();
            let url_pills = (!is_user)
                .then(|| render_url_pills(&msg.text, &format!("exp-{msg_idx}"), theme))
                .flatten();
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
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(div().text_sm().child(SharedString::from(body_text)))
                            .when_some(url_pills, |this, pills| this.child(pills)),
                    )
                    .into_any_element(),
            );
        }

        if !streaming_preview.is_empty() {
            let url_pills = render_url_pills(streaming_preview, "exp-stream", theme);
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
                            .flex()
                            .flex_col()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_sm()
                                    .child(SharedString::from(preview_text(streaming_preview))),
                            )
                            .when_some(url_pills, |this, pills| this.child(pills)),
                    )
                    .into_any_element(),
            );
        }

        if hub
            .model
            .chat
            .turn
            .as_ref()
            .and_then(|t| t.status.as_known())
            == Some(w::TurnStatus::Failed)
            && streaming_preview.is_empty()
        {
            let msg = hub
                .model
                .chat
                .turn
                .as_ref()
                .and_then(|t| t.failure.as_option())
                .map(|f| f.message.clone())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "応答の生成に失敗しました".to_owned());
            rows.push(
                div()
                    .w_full()
                    .rounded(px(12.))
                    .bg(theme.danger_soft)
                    .px(px(12.))
                    .py(px(8.))
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child(format!("エラー: {msg}"))
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
        let (theme, live, has_companion, unsent) = {
            let hub = self.hub.read(cx);
            (
                hub.theme(),
                hub.live(),
                hub.model.companion().is_some(),
                hub.model.chat.unsent.clone(),
            )
        };

        let look = theme.field_look(true, true);
        self.chat_input
            .update(cx, |draft, _| draft.apply_look(look));

        let draft_text = self.chat_input.read(cx).text().to_owned();
        let too_long = draft_text.chars().count() > MAX_INPUT;
        let can_send = live && has_companion && !draft_text.trim().is_empty() && !too_long;

        let mut unsent_rows: Vec<AnyElement> = Vec::new();
        for (idx, item) in unsent.into_iter().enumerate() {
            let key = item.request_key;
            let text = item.text;
            let reason = item.reason;
            let retry_text = text.clone();
            let dismiss_key = key.clone();
            let hub_retry = self.hub.clone();
            let hub_dismiss = self.hub.clone();
            unsent_rows.push(
                div()
                    .w_full()
                    .rounded(px(14.))
                    .bg(theme.danger_soft)
                    .px(px(12.))
                    .py(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.danger_fg)
                                    .truncate()
                                    .child(format!("未送信: {text}")),
                            )
                            .when(!reason.trim().is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.ink_muted)
                                        .truncate()
                                        .child(reason),
                                )
                            }),
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
                    .child(pill_button(
                        SharedString::from(format!("pod-dismiss-unsent-{idx}")),
                        "×",
                        None,
                        Tone::Ghost,
                        true,
                        &theme,
                        move |_, cx| {
                            hub_dismiss.update(cx, |hub, cx| {
                                hub.dismiss_unsent(&dismiss_key, cx);
                            });
                        },
                    ))
                    .into_any_element(),
            );
        }

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
            );

        let controls_open = self.controls_open;
        let pod = cx.entity();
        let trigger = floating_orb_button(
            "pod-controls-toggle",
            Icon::More,
            controls_open,
            &theme,
            move |_, cx| {
                pod.update(cx, |this, cx| {
                    // Outside-click dismissal runs earlier in the same event.
                    this.controls_open = !controls_open;
                    this.roster_open = false;
                    cx.notify();
                });
            },
        );

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
                    .relative()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(capsule)
                    .child(trigger)
                    .when(controls_open, |this| {
                        this.child(self.render_controls(&theme, cx))
                    }),
            )
            .into_any_element()
    }

    fn render_controls(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let mic_ready = matches!(hub.model.devices.input, Device::Ready(_))
            && hub.has_audio_connection(w::ConnectionKind::Stt);
        let speaker_ready = matches!(hub.model.devices.output, Device::Ready(_))
            && hub.has_audio_connection(w::ConnectionKind::Tts);
        let mic_on = mic_ready && !hub.model.mic_muted;
        let is_ptt = hub.model.capture_mode == crate::audio::CaptureMode::Ptt;
        let ptt_active = hub.model.ptt_active;
        let speaker_on = hub.live() && speaker_ready && !hub.model.output_muted;
        let expanded = hub.pod_mode == PodMode::ExpandedChat;
        let button = |id, label, icon, tone, action| {
            let pod = cx.entity();
            pill_button(
                id,
                label,
                Some(icon),
                tone,
                true,
                theme,
                move |window, cx| {
                    pod.update(cx, |this, cx| this.use_control(action, window, cx));
                },
            )
        };
        div()
            .id("pod-controls")
            .absolute()
            .bottom(px(52.))
            .right(px(0.))
            .occlude()
            .rounded(px(18.))
            .border_2()
            .border_color(theme.border_float)
            .bg(theme.bg_surface)
            .shadow(theme.float_shadow())
            .p(px(8.))
            .flex()
            .flex_col()
            .gap(px(5.))
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.controls_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .gap(px(5.))
                    .child(button(
                        "pod-control-companions",
                        "コンパニオン",
                        Icon::UserBook,
                        Tone::Ghost,
                        ChatControl::Companions,
                    ))
                    .child(button(
                        "pod-control-mic",
                        if !mic_ready {
                            "マイク設定"
                        } else if mic_on {
                            "マイクをオフ"
                        } else {
                            "マイクをオン"
                        },
                        if mic_on { Icon::Mic } else { Icon::MicOff },
                        if mic_on { Tone::Mint } else { Tone::Ghost },
                        ChatControl::Microphone { ready: mic_ready },
                    ))
                    .child(button(
                        "pod-control-speaker",
                        if !speaker_ready {
                            "音声設定"
                        } else if speaker_on {
                            "音声をオフ"
                        } else {
                            "音声をオン"
                        },
                        if speaker_on {
                            Icon::Volume
                        } else {
                            Icon::VolumeOff
                        },
                        if speaker_on { Tone::Mint } else { Tone::Ghost },
                        ChatControl::Speaker {
                            ready: speaker_ready,
                        },
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap(px(5.))
                    .child(button(
                        "pod-control-history",
                        if expanded {
                            "履歴を閉じる"
                        } else {
                            "履歴を表示"
                        },
                        if expanded {
                            Icon::ChatCollapse
                        } else {
                            Icon::ChatBubble
                        },
                        Tone::Ghost,
                        ChatControl::History,
                    ))
                    .child(button(
                        "pod-control-capture-mode",
                        if is_ptt {
                            "PTT モード"
                        } else {
                            "VAD モード"
                        },
                        Icon::Mic,
                        if is_ptt { Tone::Primary } else { Tone::Ghost },
                        ChatControl::CaptureMode,
                    ))
                    .when(is_ptt && mic_on, |row| {
                        row.child(button(
                            "pod-control-ptt-toggle",
                            if ptt_active {
                                "送信 (話す終了)"
                            } else {
                                "押して話す"
                            },
                            Icon::Mic,
                            if ptt_active {
                                Tone::Mint
                            } else {
                                Tone::Primary
                            },
                            ChatControl::PttToggle,
                        ))
                        .when(ptt_active, |row| {
                            row.child(button(
                                "pod-control-ptt-cancel",
                                "キャンセル",
                                Icon::Close,
                                Tone::Danger,
                                ChatControl::PttCancel,
                            ))
                        })
                    })
                    .child(button(
                        "pod-control-settings",
                        "設定",
                        Icon::Settings,
                        Tone::Ghost,
                        ChatControl::Settings,
                    ))
                    .child(button(
                        "pod-control-close",
                        "チャットを閉じる",
                        Icon::Close,
                        Tone::Danger,
                        ChatControl::Close,
                    )),
            )
            .into_any_element()
    }

    fn use_control(&mut self, action: ChatControl, window: &mut Window, cx: &mut Context<Self>) {
        self.controls_open = false;
        cx.notify();
        match action {
            ChatControl::Companions => self.roster_open = true,
            ChatControl::History => self.toggle_mode(window, cx),
            ChatControl::Settings => self.hub.update(cx, |hub, cx| {
                hub.open_settings(SettingsSection::Companions, cx)
            }),
            ChatControl::Microphone { ready } => self.hub.update(cx, |hub, cx| {
                if ready {
                    hub.toggle_mic_mute(cx)
                } else {
                    hub.open_settings(SettingsSection::Device, cx)
                }
            }),
            ChatControl::CaptureMode => self.hub.update(cx, |hub, cx| hub.toggle_capture_mode(cx)),
            ChatControl::PttToggle => self.hub.update(cx, |hub, cx| hub.ptt_toggle(cx)),
            ChatControl::PttCancel => self.hub.update(cx, |hub, cx| hub.ptt_cancel(cx)),
            ChatControl::Speaker { ready } => self.hub.update(cx, |hub, cx| {
                if ready {
                    hub.toggle_output_mute(cx)
                } else {
                    hub.open_settings(SettingsSection::Device, cx)
                }
            }),
            ChatControl::Close => {
                let hub = self.hub.read(cx);
                if hub.is_tray_resident() || hub.settings_window.is_some() {
                    self.hub.update(cx, |hub, cx| hub.hide_pod(cx));
                } else {
                    cx.quit();
                }
            }
        }
    }

    fn render_roster_popover(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let hub = self.hub.read(cx);
        let companions = hub
            .model
            .snapshot()
            .map(|s| s.companions.clone())
            .unwrap_or_default();
        let active_id = hub.model.companion_id().map(str::to_owned);
        let client_id = hub.options.id.clone();

        let mut rows: Vec<AnyElement> = Vec::new();
        for comp in companions {
            let cid = comp.id.clone();
            let active = active_id.as_deref() == Some(cid.as_str());
            let resident = hub.model.is_resident(&cid, &client_id);
            let persona = comp
                .persona
                .as_deref()
                .map(|s| s.trim().replace('\n', " "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "話し方未設定".to_owned());
            let pick_id = cid.clone();
            let toggle_id = cid.clone();
            let hub_res = self.hub.clone();
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
                        (active || resident).then_some(theme.mint),
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
                    .child(pill_button(
                        SharedString::from(format!("pod-roster-res-{cid}")),
                        if resident { "常駐中" } else { "常駐" },
                        None,
                        if resident { Tone::Mint } else { Tone::Ghost },
                        true,
                        theme,
                        move |_, cx| {
                            cx.stop_propagation();
                            hub_res.update(cx, |hub, cx| {
                                if resident {
                                    hub.release_companion(toggle_id.clone(), cx);
                                } else {
                                    hub.activate_companion(toggle_id.clone(), cx);
                                }
                            });
                        },
                    ))
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
            .right(px(14.))
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
                apply_hwnd_topmost(hwnd);
            }
        }
        let needs_key = self.needs_connection(cx);
        let in_onboarding = needs_key || self.in_model_setup(cx);
        if in_onboarding != self.key_focused {
            self.key_focused = in_onboarding;
            self.controls_open = false;
            self.focus_input(window, cx);
        }
        let desired = self.desired_size(cx);
        let (scroll_ping, pod_visible, theme) = {
            let hub = self.hub.read(cx);
            (hub.pod_scroll_ping, hub.is_pod_visible(), hub.theme())
        };
        self.apply_size(desired, window, cx);
        if pod_visible {
            ensure_pod_topmost();
        }
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
            .when(!in_onboarding, |root| {
                root.child(self.render_capsule_dock(cx))
            })
            .when(roster_open, |this| {
                this.child(self.render_roster_popover(&theme, cx))
            })
    }
}

/// Binds keyboard shortcuts (`Enter` send in `ChatInput`, `Esc` stop/collapse) globally.
pub fn bind(cx: &mut App, hub: Entity<Hub>) {
    input::bind(cx);
    cx.bind_keys([
        KeyBinding::new("enter", Submit, Some("ChatInput")),
        KeyBinding::new("escape", CancelOrStop, None),
    ]);

    let submit_hub = hub.clone();
    cx.on_action(move |_: &Submit, cx: &mut App| {
        let Some(view) = submit_hub.read(cx).pod_view() else {
            return;
        };
        let _ = view.update(cx, |this, cx| this.submit(cx));
    });

    cx.on_action(move |_: &CancelOrStop, cx: &mut App| {
        let Some(view) = hub.read(cx).pod_view() else {
            return;
        };
        let _ = view.update(cx, |this, cx| {
            if this.controls_open {
                this.controls_open = false;
                cx.notify();
                return;
            }
            if this.setup_model_dropdown_open || this.setup_voice_dropdown_open {
                this.setup_model_dropdown_open = false;
                this.setup_voice_dropdown_open = false;
                cx.notify();
                return;
            }
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

pub(crate) fn set_pod_window_visible(visible: bool) {
    #[cfg(windows)]
    if let Some(hwnd) = current_pod_hwnd() {
        use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow};
        unsafe {
            let _ = ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
        }
        if visible {
            apply_hwnd_topmost(hwnd);
        }
    }
    #[cfg(not(windows))]
    let _ = visible;
}

pub(crate) fn ensure_pod_topmost() {
    #[cfg(windows)]
    if let Some(hwnd) = current_pod_hwnd() {
        apply_hwnd_topmost(hwnd);
    }
}

fn active_mini_speech_text(model: &crate::state::Model) -> String {
    let streaming: String = model
        .streaming
        .iter()
        .filter(|((c, _), _)| c == &model.chat.companion_id)
        .map(|(_, text)| text.as_str())
        .collect();
    if !streaming.is_empty() {
        return streaming;
    }
    if !model.chat.sending.is_empty()
        || model
            .chat
            .turn
            .as_ref()
            .is_some_and(|t| t.status == w::TurnStatus::Streaming)
    {
        return "返答を考えています…".to_owned();
    }
    model
        .chat
        .messages
        .iter()
        .rev()
        .find(|m| m.role != w::MessageRole::User)
        .map(|m| m.text.clone())
        .unwrap_or_else(|| "こんにちは。まずはひとこと送ってみてください。".to_owned())
}

fn is_motion_leak_line(line: &str) -> bool {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(line) else {
        return false;
    };
    !items.is_empty()
        && items.iter().all(|item| {
            item.as_object().is_some_and(|obj| {
                !obj.is_empty()
                    && obj.iter().all(|(key, value)| {
                        matches!(key.as_str(), "motion" | "expression") && value.is_string()
                    })
            })
        })
}

fn strip_motion_leak(raw: &str) -> String {
    let trimmed = raw.trim();
    let first = trimmed.lines().next().unwrap_or("");
    if is_motion_leak_line(first) {
        trimmed[first.len()..].trim_start().to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn preview_text(raw: &str) -> String {
    // Hold an unfinished leading motion-control JSON line until its shape is known,
    // while preserving Markdown, URLs, code blocks and emojis verbatim.
    let (complete, tail) = raw.rsplit_once('\n').unwrap_or(("", raw));
    if complete.is_empty() && tail.trim_start().starts_with('[') {
        return String::new();
    }
    if tail.trim_start().starts_with('[') && !tail.trim_end().ends_with(']') {
        strip_motion_leak(complete)
    } else {
        strip_motion_leak(raw)
    }
}

fn short_url_label(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let trimmed = without_scheme.trim_end_matches('/');
    if trimmed.chars().count() <= 32 {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(29).collect();
        format!("{head}…")
    }
}

fn extract_urls(raw: &str) -> Vec<(String, String)> {
    let mut urls: Vec<(String, String)> = Vec::new();
    let mut push_unique = |label: String, url: String| {
        if urls.len() >= 4 || urls.iter().any(|(_, existing)| existing == &url) {
            return;
        }
        let clean_label = if label.trim().is_empty() {
            short_url_label(&url)
        } else {
            label
        };
        urls.push((clean_label, url));
    };

    let mut rest = raw;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        if let Some(mid) = after_open.find("](")
            && let Some(close) = after_open[mid + 2..].find(')')
        {
            let label = after_open[..mid].trim();
            let url = after_open[mid + 2..mid + 2 + close].trim();
            if url.starts_with("http://") || url.starts_with("https://") {
                push_unique(label.to_owned(), url.to_owned());
                rest = &after_open[mid + 2 + close + 1..];
                continue;
            }
        }
        rest = after_open;
    }

    for token in raw.split_whitespace() {
        let candidate = token.trim_start_matches(['(', '[', '<', '"', '\'', '「', '『']);
        if let Some(pos) = candidate
            .find("https://")
            .or_else(|| candidate.find("http://"))
        {
            let slice = &candidate[pos..];
            let end = slice
                .find(|ch: char| {
                    matches!(
                        ch,
                        ')' | ']' | '>' | '"' | '\'' | ',' | '。' | '、' | '」' | '』'
                    ) || ch.is_whitespace()
                })
                .unwrap_or(slice.len());
            let clean = slice[..end].trim_end_matches(['.', ';', ':']);
            if clean.starts_with("http://") || clean.starts_with("https://") {
                push_unique(short_url_label(clean), clean.to_owned());
            }
        }
    }
    urls
}

fn render_url_pills(raw: &str, id_prefix: &str, theme: &Theme) -> Option<AnyElement> {
    let urls = extract_urls(raw);
    if urls.is_empty() {
        return None;
    }
    let chips: Vec<AnyElement> = urls
        .into_iter()
        .enumerate()
        .map(|(idx, (label, url))| {
            let hover_bg = theme.accent_soft;
            let open_url = url.clone();
            div()
                .id(SharedString::from(format!("{id_prefix}-url-{idx}")))
                .rounded_full()
                .bg(theme.accent_soft)
                .border_1()
                .border_color(theme.border)
                .px(px(8.))
                .py(px(2.))
                .text_xs()
                .text_color(theme.accent_soft_fg)
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .on_click(move |_, _, cx| {
                    cx.open_url(&open_url);
                })
                .child(label)
                .into_any_element()
        })
        .collect();
    Some(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .children(chips)
            .into_any_element(),
    )
}

#[cfg(windows)]
fn apply_hwnd_topmost(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowPos,
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
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
            GetWindowRect, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE, SetWindowPos,
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
            Some(HWND_TOPMOST),
            rect.left,
            new_top,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

#[cfg(windows)]
fn suppress_dwm_frame(hwnd: windows::Win32::Foundation::HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (style & !(WS_EX_TOOLWINDOW.0 as isize)) | WS_EX_APPWINDOW.0 as isize,
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_holds_partial_motion_data_and_preserves_markdown() {
        assert_eq!(preview_text("[{\"motion\":"), "");
        assert_eq!(
            preview_text("[{\"motion\":\"nod\"}]\nこんにちは！"),
            "こんにちは！"
        );
    }

    #[test]
    fn preview_preserves_markdown_links_and_extract_urls_parses_them() {
        let raw = "### 結果\n- **公式**: [Antigravity](https://example.com/docs) を確認 👍\nhttps://example.com/raw。";
        let clean = preview_text(raw);
        assert!(clean.contains("**公式**"), "{clean}");
        assert!(
            clean.contains("[Antigravity](https://example.com/docs)"),
            "{clean}"
        );
        assert!(clean.contains("👍"), "{clean}");
        let urls = extract_urls(raw);
        assert_eq!(
            urls,
            vec![
                (
                    "Antigravity".to_owned(),
                    "https://example.com/docs".to_owned()
                ),
                (
                    "example.com/raw".to_owned(),
                    "https://example.com/raw".to_owned()
                ),
            ]
        );
    }
}
