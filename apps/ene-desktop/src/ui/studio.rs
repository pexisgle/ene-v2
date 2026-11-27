//! Standalone `設定 — ene` window with 38px Strawberry-Milk titlebar,
//! 196px four-chapter sidebar, and flat content pane.

use super::companions::{CompanionsPane, voice::VoicePane};
use super::connections::ConnectionsPane;
use super::device::DevicePane;
use super::icons::Icon;
use super::theme::{Theme, resolve_ui_font_family, ui_font_fallbacks};
use super::widgets::{
    CircleButtonStyle, Tone, circle_icon_button, navigation_button, pill_button, window_drag_area,
};
use super::{Hub, SettingsSection};
use crate::state::NoticeKind;
use gpui::{AnyElement, Context, Entity, SharedString, Window, div, prelude::*, px};

/// Settings window managing companions, connections and local device permissions.
pub struct SettingsStudio {
    pub(crate) hub: Entity<Hub>,
    companions: Entity<CompanionsPane>,
    connections: Entity<ConnectionsPane>,
    voice: Entity<VoicePane>,
    device: Entity<DevicePane>,
    content_scroll: gpui::ScrollHandle,
    shown_section: SettingsSection,
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
        let shown_section = hub.read(cx).windows.settings_section;
        Self {
            hub,
            companions,
            connections,
            voice,
            device,
            content_scroll: gpui::ScrollHandle::new(),
            shown_section,
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
                        CircleButtonStyle::new(Icon::WindowMinimize, 24., 12.),
                        theme,
                        |window, _| window.minimize_window(),
                    ))
                    .child(circle_icon_button(
                        "studio-win-max",
                        CircleButtonStyle::new(Icon::WindowMaximize, 24., 12.),
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

    fn render_sidebar(&self, active: SettingsSection, theme: &Theme) -> AnyElement {
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
            .children(SettingsSection::ALL.into_iter().map(|section| {
                let hub = self.hub.clone();
                navigation_button(
                    section.key(),
                    section.label(),
                    section.icon(),
                    section == active,
                    theme,
                    move |_, cx| {
                        hub.update(cx, |hub, cx| hub.select_settings_section(section, cx));
                    },
                )
            }))
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
                            Tone::Soft.icon(Icon::RefreshCw),
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

        if section != self.shown_section {
            match self.shown_section {
                SettingsSection::Connections => {
                    self.connections.update(cx, |pane, _| pane.on_leave())
                }
                SettingsSection::Device => self.device.update(cx, |pane, _| pane.on_leave()),
                _ => {}
            }
            self.content_scroll.set_offset(gpui::point(px(0.), px(0.)));
            self.shown_section = section;
        }
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
                    .child(self.render_sidebar(section, &theme))
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
