use super::{CompanionPod, MAX_INPUT};
use crate::state::Unsent;
use crate::ui::hub::chat;
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    ButtonStyle, CircleButtonStyle, Tone, circle_icon_button, companion_badge, floating_orb_button,
    pill_button,
};
use crate::ui::{Hub, SettingsSection};
use ene_client_core::domain::Companion;
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, SharedString, div, prelude::*, px,
};

pub(super) fn render_capsule_dock(
    pod: &mut CompanionPod,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let (theme, live, has_companion, unsent) = {
        let hub = pod.hub.read(cx);
        (
            hub.theme(),
            hub.live(),
            hub.model.companion().is_some(),
            hub.model.companions.chat.unsent.clone(),
        )
    };

    let look = theme.field_look(true);
    pod.chat_input
        .update(cx, |draft, cx| draft.apply_look(look, cx));

    let draft_text = pod.chat_input.read(cx).text().to_owned();
    let too_long = draft_text.chars().count() > MAX_INPUT;
    let can_send = live && has_companion && !draft_text.trim().is_empty() && !too_long;
    let unsent_rows = render_unsent_rows(unsent, live, &pod.hub, &theme);
    let controls_open = pod.controls_open;
    let pod_entity = cx.entity();
    let trigger = floating_orb_button(
        "pod-controls-toggle",
        Icon::Ellipsis,
        controls_open,
        &theme,
        move |_, cx| {
            pod_entity.update(cx, |this, cx| {
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
                .child(render_input_capsule(pod, can_send, &theme, cx))
                .child(trigger)
                .when(controls_open, |this| {
                    this.child(super::menu::render_controls(pod, &theme, cx))
                }),
        )
        .into_any_element()
}

fn render_input_capsule(
    pod: &CompanionPod,
    can_send: bool,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> gpui::Div {
    let pod_entity = cx.entity();
    div()
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
                .child(pod.chat_input.clone()),
        )
        .child(circle_icon_button(
            "pod-send-btn",
            "メッセージを送信",
            CircleButtonStyle::new(Icon::Send, 30., 14.)
                .tone(Tone::Primary)
                .enabled(can_send),
            theme,
            move |window, cx| pod_entity.update(cx, |pod, cx| pod.submit(window, cx)),
        ))
}

pub(super) fn render_roster_popover(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let hub = pod.hub.read(cx);
    let companions = hub
        .model
        .snapshot()
        .map_or_default(|s| s.companions.clone());
    let active_id = hub.model.companion_id().map(str::to_owned);
    let client_id = hub.options.id.clone();

    let mut rows: Vec<AnyElement> = companions
        .into_iter()
        .map(|comp| {
            let active = active_id.as_deref() == Some(comp.id.as_str());
            let resident = hub.model.is_resident(&comp.id, &client_id);
            render_roster_row(comp, active, resident, &pod.hub, theme, cx)
        })
        .collect();

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
        .rounded(crate::ui::theme::radius::orb())
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
        .child(render_roster_footer(&pod.hub, theme, cx))
        .into_any_element()
}

fn render_unsent_rows(
    unsent: Vec<Unsent>,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> Vec<AnyElement> {
    unsent
        .into_iter()
        .enumerate()
        .map(|(idx, item)| {
            let key = item.request_key;
            let text = item.text;
            let reason = item.reason;
            let retry_text = text.clone();
            let dismiss_key = key.clone();
            let hub_retry = hub.clone();
            let hub_dismiss = hub.clone();
            div()
                .w_full()
                .rounded(crate::ui::theme::radius::surface())
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
                    Tone::Primary.icon(Icon::RefreshCw).enabled(live),
                    theme,
                    move |_, cx| {
                        hub_retry
                            .update(cx, |hub, cx| chat::retry_unsent(hub, &key, &retry_text, cx));
                    },
                ))
                .child(circle_icon_button(
                    SharedString::from(format!("pod-dismiss-unsent-{idx}")),
                    "未送信メッセージを破棄",
                    CircleButtonStyle::new(Icon::Close, 22., 11.),
                    theme,
                    move |_, cx| {
                        hub_dismiss
                            .update(cx, |hub, cx| chat::dismiss_unsent(hub, &dismiss_key, cx));
                    },
                ))
                .into_any_element()
        })
        .collect()
}

fn render_roster_row(
    comp: Companion,
    active: bool,
    resident: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
    cx: &Context<CompanionPod>,
) -> AnyElement {
    let cid = comp.id.clone();
    let persona = comp
        .persona
        .as_deref()
        .map(|s| s.trim().replace('\n', " "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "話し方未設定".to_owned());
    let pick_id = cid.clone();
    let toggle_id = cid.clone();
    let hub_res = hub.clone();

    div()
        .id(SharedString::from(format!("pod-roster-{}", comp.id)))
        .px(px(10.))
        .py(px(8.))
        .rounded(crate::ui::theme::radius::large())
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
                    .update(cx, |hub, cx| chat::select_companion(hub, &pick_id, cx));
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
            ButtonStyle::new(if resident { Tone::Mint } else { Tone::Ghost }),
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
        .into_any_element()
}

fn render_roster_footer(hub: &Entity<Hub>, theme: &Theme, cx: &Context<CompanionPod>) -> gpui::Div {
    let hub_manage = hub.clone();
    let pod_close = cx.entity();
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
            Tone::Ghost.icon(Icon::Settings),
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
        ))
}
