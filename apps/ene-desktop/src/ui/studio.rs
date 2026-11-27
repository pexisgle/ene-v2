//! Standalone `設定 — ene` window with 38px Strawberry-Milk titlebar,
//! 196px 3-chapter sidebar (`コンパニオン`, `接続`, `この端末`), and flat content pane.

use super::companions::{CompanionsPane, voice::VoicePane};
use super::connections::ConnectionsPane;
use super::device::DevicePane;
use super::icons::{Icon, icon};
use super::theme::{Theme, resolve_ui_font_family, ui_font_fallbacks};
use super::widgets::{CircleButtonStyle, Tone, circle_icon_button, pill_button, window_drag_area};
use super::{Hub, SettingsSection};
use crate::state::NoticeKind;
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};

/// Settings window managing companions, connections and local device permissions.
pub struct SettingsStudio {
    pub(crate) hub: Entity<Hub>,
    companions: Entity<CompanionsPane>,
    connections: Entity<ConnectionsPane>,
    voice: Entity<VoicePane>,
    device: Entity<DevicePane>,
    content_scroll: gpui::ScrollHandle,
}

impl std::fmt::Debug for SettingsStudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStudio").finish_non_exhaustive()
    }
}

impl SettingsStudio {
    pub fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let companions = cx.new(|cx| CompanionsPane::new(hub.clone(), cx));
        let connections = cx.new(|cx| ConnectionsPane::new(hub.clone(), cx));
        let voice = cx.new(|cx| VoicePane::new(hub.clone(), cx));
        let device = cx.new(|_| DevicePane::new(hub.clone()));
        Self {
            hub,
            companions,
            connections,
            voice,
            device,
            content_scroll: gpui::ScrollHandle::new(),
        }
    }

    fn render_titlebar(&self, theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .w_full()
            .h(px(38.))
            .bg(theme.bg_milk)
            .border_b_1()
            .border_color(theme.border_title)
            .px(px(14.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                window_drag_area(div().flex_1().h_full().flex().items_center(), true).child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child("設定 — ene"),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(circle_icon_button(
                        "studio-win-min",
                        CircleButtonStyle::new(Icon::Minimize, 24., 12.),
                        theme,
                        |window, _| window.minimize_window(),
                    ))
                    .child(circle_icon_button(
                        "studio-win-max",
                        CircleButtonStyle::new(Icon::Maximize, 24., 12.),
                        theme,
                        |window, _| window.zoom_window(),
                    ))
                    .child(circle_icon_button(
                        "studio-win-close",
                        CircleButtonStyle::new(Icon::Close, 24., 12.).tone(Tone::Danger),
                        theme,
                        |window, _| window.remove_window(),
                    )),
            )
            .into_any_element()
    }

    fn render_sidebar(
        &self,
        active: SettingsSection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut items: Vec<AnyElement> = Vec::new();
        for section in SettingsSection::ALL {
            let is_active = section == active;
            let hub = self.hub.clone();
            let studio = cx.entity();
            items.push(
                div()
                    .id(SharedString::from(section.key()))
                    .w_full()
                    .px(px(14.))
                    .py(px(9.))
                    .rounded_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .cursor_pointer()
                    .bg(if is_active {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
                    .when(!is_active, |this| {
                        this.hover(|this| this.bg(theme.bg_surface.alpha(0.65)))
                    })
                    .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, _, cx| {
                        studio.update(cx, |this, cx| {
                            this.companions.update(cx, |p, _| p.on_leave());
                            this.connections.update(cx, |p, _| p.on_leave());
                            this.device.update(cx, |p, _| p.on_leave());
                            this.content_scroll.set_offset(gpui::point(px(0.), px(0.)));
                        });
                        hub.update(cx, |hub, cx| {
                            hub.windows.settings_section = section;
                            if section == SettingsSection::Companions {
                                hub.load_companion_templates(cx);
                            } else if section == SettingsSection::Connections {
                                hub.load_providers(cx);
                                hub.load_models(cx);
                                hub.load_plugins(cx);
                            } else if section == SettingsSection::Voice {
                                hub.load_providers(cx);
                            } else if section == SettingsSection::Device {
                                hub.load_plugins(cx);
                            }
                            cx.notify();
                        });
                    })
                    .child(icon(
                        section.icon(),
                        15.,
                        if is_active {
                            theme.accent_fg
                        } else {
                            theme.ink_muted
                        },
                    ))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(if is_active {
                                gpui::FontWeight::BOLD
                            } else {
                                gpui::FontWeight::MEDIUM
                            })
                            .text_color(if is_active {
                                theme.accent_fg
                            } else {
                                theme.ink
                            })
                            .child(section.label()),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex_none()
            .w(px(196.))
            .h_full()
            .bg(theme.bg_milk)
            .border_r_1()
            .border_color(theme.border_title)
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .children(items)
            .into_any_element()
    }

    fn render_notices(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let notices = self.hub.read(cx).model.notices.clone();
        if notices.is_empty() {
            return None;
        }
        let mut strips: Vec<AnyElement> = Vec::new();
        for (idx, notice) in notices.into_iter().enumerate() {
            let is_conflict = notice.kind == NoticeKind::Conflict;
            let hub_reload = self.hub.clone();
            let hub_dismiss = self.hub.clone();
            strips.push(
                div()
                    .w_full()
                    .rounded(crate::ui::theme::radius::large())
                    .bg(if is_conflict {
                        theme.amber_soft
                    } else {
                        theme.danger_soft
                    })
                    .px(px(12.))
                    .py(px(8.))
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
                            .child(format!("{}: {}", notice.context, notice.message)),
                    )
                    .when(is_conflict, |this| {
                        this.child(pill_button(
                            SharedString::from(format!("studio-notice-reload-{idx}")),
                            "再読み込み",
                            Tone::Soft.icon(Icon::Refresh),
                            theme,
                            move |_, cx| {
                                hub_reload.update(cx, |hub, cx| {
                                    hub.refresh_active_data(cx);
                                    hub.dismiss_notice(idx, cx);
                                });
                            },
                        ))
                    })
                    .child(circle_icon_button(
                        SharedString::from(format!("studio-notice-close-{idx}")),
                        CircleButtonStyle::new(Icon::Close, 22., 11.),
                        theme,
                        move |_, cx| {
                            hub_dismiss.update(cx, |hub, cx| hub.dismiss_notice(idx, cx));
                        },
                    ))
                    .into_any_element(),
            );
        }

        Some(
            div()
                .flex_none()
                .px(px(28.))
                .pt(px(16.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(strips)
                .into_any_element(),
        )
    }
}

impl Render for SettingsStudio {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (section, theme) = {
            let hub = self.hub.read(cx);
            (hub.windows.settings_section, hub.theme())
        };

        let notice_block = self.render_notices(&theme, cx);
        let section_body: AnyElement = match section {
            SettingsSection::Companions => self.companions.clone().into_any_element(),
            SettingsSection::Connections => self.connections.clone().into_any_element(),
            SettingsSection::Voice => self.voice.clone().into_any_element(),
            SettingsSection::Device => self.device.clone().into_any_element(),
        };

        div()
            .size_full()
            .bg(theme.bg_surface)
            .text_color(theme.ink)
            .font({
                let mut f = gpui::font(resolve_ui_font_family(cx));
                f.fallbacks = Some(ui_font_fallbacks());
                f
            })
            .flex()
            .flex_col()
            .child(self.render_titlebar(&theme))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_sidebar(section, &theme, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .bg(theme.bg_surface)
                            .flex()
                            .flex_col()
                            .when_some(notice_block, gpui::ParentElement::child)
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "studio-content-scroll-{}",
                                        section.key()
                                    )))
                                    .track_scroll(&self.content_scroll)
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .px(px(28.))
                                    .py(px(22.))
                                    .child(section_body),
                            ),
                    ),
            )
    }
}

/// Shared clean field block with label and zero decorative kickers.
pub(crate) fn labeled_field(
    label: &'static str,
    field: impl gpui::IntoElement,
    theme: &Theme,
) -> AnyElement {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child(label),
        )
        .child(field)
        .into_any_element()
}
