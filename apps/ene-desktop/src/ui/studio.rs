//! Standalone `設定 — ene` window with 38px Strawberry-Milk titlebar,
//! 196px 3-chapter sidebar (`コンパニオン`, `接続`, `この端末`), and flat content pane.

use super::icons::{Icon, icon};
use super::theme::{Theme, resolve_ui_font_family, ui_font_fallbacks};
use super::widgets::{CircleButtonStyle, Tone, circle_icon_button, pill_button, window_drag_area};
use super::{DraftSpec, Hub, SettingsSection, window_draft};
use crate::{input::Draft, state::NoticeKind};
use ene_client_core::domain::{ConnectionKind, PolicyEffect};
use gpui::{
    AnyElement, Context, Entity, MouseButton, MouseDownEvent, SharedString, Subscription, Window,
    div, prelude::*, px,
};
use std::collections::{BTreeMap, HashSet};

/// Form and expansion state for the Companions tab in [`SettingsStudio`].
#[derive(Default)]
pub(crate) struct CompanionFormState {
    pub(crate) creating_companion: bool,
    pub(crate) importing_template: bool,
    pub(crate) preview_pending: bool,
    pub(crate) expanded_companion_id: Option<String>,
    pub(crate) mind_disclosure_open: bool,
    pub(crate) delete_armed_companion: Option<String>,
    last_created_ping: u64,
}

/// Form and disclosure state for the Connections tab in [`SettingsStudio`].
pub(crate) struct ConnectionFormState {
    pub(crate) adding_connection: bool,
    pub(crate) custom_connection_open: bool,
    pub(crate) custom_kind: ConnectionKind,
    pub(crate) mcp_add_mode: super::mcp_add::Mode,
    pub(crate) mcp_add_advanced_open: bool,
    pub(crate) pending_mcp: Option<String>,
    pub(crate) expanded_connection_id: Option<String>,
    pub(crate) revealed_secret_for: Option<String>,
    pub(crate) revoke_armed_for: Option<String>,
    pub(crate) advanced_roles_open: bool,
    pub(crate) auto_probed_connections: HashSet<String>,
    last_cleared_secret_seq: u64,
}

impl Default for ConnectionFormState {
    fn default() -> Self {
        Self {
            adding_connection: false,
            custom_connection_open: false,
            custom_kind: ConnectionKind::Llm,
            mcp_add_mode: super::mcp_add::Mode::Npx,
            mcp_add_advanced_open: false,
            pending_mcp: None,
            expanded_connection_id: None,
            revealed_secret_for: None,
            revoke_armed_for: None,
            advanced_roles_open: false,
            auto_probed_connections: HashSet::new(),
            last_cleared_secret_seq: 0,
        }
    }
}

/// Form and arming state for the Device & Policy tab in [`SettingsStudio`].
pub(crate) struct DeviceFormState {
    pub(crate) delete_armed_policy: Option<String>,
    pub(crate) adding_policy_open: bool,
    pub(crate) new_policy_effect: PolicyEffect,
    pub(crate) delete_armed_policy_text: Option<String>,
    pub(crate) editing_policy_text_id: Option<String>,
    pub(crate) adding_policy_text_open: bool,
    pub(crate) adding_plugin_open: bool,
    pub(crate) remote_host_open: bool,
    pub(crate) skip_confirm_armed: bool,
}

impl Default for DeviceFormState {
    fn default() -> Self {
        Self {
            delete_armed_policy: None,
            adding_policy_open: false,
            new_policy_effect: PolicyEffect::Allow,
            delete_armed_policy_text: None,
            editing_policy_text_id: None,
            adding_policy_text_open: false,
            adding_plugin_open: false,
            remote_host_open: false,
            skip_confirm_armed: false,
        }
    }
}

/// Settings window managing companions, connections and local device permissions.
pub struct SettingsStudio {
    pub(crate) hub: Entity<Hub>,
    drafts: BTreeMap<String, Entity<Draft>>,
    initialized: bool,
    pub(crate) companion_form: CompanionFormState,
    pub(crate) connection_form: ConnectionFormState,
    pub(crate) device_form: DeviceFormState,
    pub(crate) open_dropdown: Option<String>,
    content_scroll: gpui::ScrollHandle,
    _observe: Subscription,
}

impl std::fmt::Debug for SettingsStudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsStudio")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

impl SettingsStudio {
    pub fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            drafts: BTreeMap::new(),
            initialized: false,
            companion_form: CompanionFormState::default(),
            connection_form: ConnectionFormState::default(),
            device_form: DeviceFormState::default(),
            open_dropdown: None,
            content_scroll: gpui::ScrollHandle::new(),
            _observe,
        }
    }

    pub(crate) fn draft(
        &mut self,
        cx: &mut Context<Self>,
        key: impl Into<String>,
        placeholder: &'static str,
        initial: Option<&str>,
        theme: &Theme,
    ) -> Entity<Draft> {
        window_draft(
            &mut self.drafts,
            cx,
            DraftSpec {
                key: key.into(),
                placeholder,
                initial,
                look: theme.field_look(false, false),
                context: "TextField",
            },
        )
    }

    pub(crate) fn description_draft(
        &mut self,
        cx: &mut Context<Self>,
        key: impl Into<String>,
        placeholder: &'static str,
        initial: Option<&str>,
        theme: &Theme,
    ) -> Entity<Draft> {
        window_draft(
            &mut self.drafts,
            cx,
            DraftSpec {
                key: key.into(),
                placeholder,
                initial,
                look: theme.field_look(false, false),
                context: "TextField Multiline",
            },
        )
    }

    pub(crate) fn clear_mcp_drafts(&mut self) {
        self.drafts.retain(|key, _| !key.starts_with("add:mcp:"));
    }

    pub(crate) fn masked_draft(
        &mut self,
        cx: &mut Context<Self>,
        key: impl Into<String>,
        placeholder: &'static str,
        masked: bool,
        theme: &Theme,
    ) -> Entity<Draft> {
        let entity = self.draft(cx, key, placeholder, None, theme);
        if entity.read(cx).is_masked() != masked {
            entity.update(cx, |draft, cx| draft.set_masked(masked, cx));
        }
        entity
    }

    fn sync_hub_events(&mut self, cx: &mut Context<Self>) {
        let (created_ping, cleared, active_comp, has_preview) = {
            let hub = self.hub.read(cx);
            (
                hub.created_ping,
                hub.cleared_secret_for.clone(),
                hub.model.companion_id().map(str::to_owned),
                hub.model.companions.preview_reply.is_some(),
            )
        };
        if has_preview {
            self.companion_form.preview_pending = false;
        }
        if !self.initialized {
            self.initialized = true;
            self.companion_form.expanded_companion_id = active_comp;
            self.companion_form.last_created_ping = created_ping;
            self.connection_form.last_cleared_secret_seq =
                cleared.as_ref().map_or(0, |(_, seq)| *seq);
            return;
        }
        if created_ping != self.companion_form.last_created_ping {
            self.companion_form.last_created_ping = created_ping;
            self.companion_form.creating_companion = false;
            self.companion_form.importing_template = false;
            self.companion_form.preview_pending = false;
            self.companion_form.expanded_companion_id = active_comp;
            for key in ["new:name", "new:mind", "new:persona", "import:json"] {
                if let Some(d) = self.drafts.get(key).cloned() {
                    d.update(cx, |draft, cx| draft.clear(cx));
                }
            }
        }
        if let Some((conn_id, seq)) = cleared
            && seq != self.connection_form.last_cleared_secret_seq
        {
            self.connection_form.last_cleared_secret_seq = seq;
            let adapter = self.hub.read(cx).model.settings().and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == conn_id)
                    .and_then(|c| c.search_adapter())
                    .map(|a| a.to_ascii_lowercase())
            });
            let mut keys = vec![format!("conn:secret:{conn_id}")];
            if let Some(adapter) = adapter {
                keys.push(format!("search-card:secret:{adapter}"));
            } else if let Some(suffix) = conn_id.strip_prefix("search-") {
                keys.push(format!("search-card:secret:{suffix}"));
            }
            for key in keys {
                if let Some(d) = self.drafts.get(&key).cloned() {
                    d.update(cx, |draft, cx| draft.clear(cx));
                }
            }
            if self.connection_form.revealed_secret_for.as_deref() == Some(conn_id.as_str())
                || self
                    .connection_form
                    .revealed_secret_for
                    .as_deref()
                    .is_some_and(|r| r.starts_with("search-card:"))
            {
                self.connection_form.revealed_secret_for = None;
            }
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
                        studio.update(cx, |this, _| {
                            this.open_dropdown = None;
                            this.device_form.skip_confirm_armed = false;
                            this.content_scroll.set_offset(gpui::point(px(0.), px(0.)));
                        });
                        hub.update(cx, |hub, cx| {
                            hub.settings_section = section;
                            if section == SettingsSection::Companions {
                                hub.load_companion_templates(cx);
                            } else if section == SettingsSection::Connections {
                                hub.load_providers(cx);
                                hub.load_models(cx);
                            } else if section == SettingsSection::Voice {
                                hub.load_providers(cx);
                            } else if section == SettingsSection::Device {
                                hub.load_policy_rules(cx);
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

        let _ = cx;

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
                    .rounded(px(12.))
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
                                    hub.resnapshot();
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
        self.sync_hub_events(cx);
        let (section, theme) = {
            let hub = self.hub.read(cx);
            (hub.settings_section, hub.theme())
        };

        let notice_block = self.render_notices(&theme, cx);
        let section_body = match section {
            SettingsSection::Companions => self.render_companions(&theme, cx),
            SettingsSection::Connections => self.render_connections(&theme, cx),
            SettingsSection::Voice => self.render_voice(&theme, cx),
            SettingsSection::Device => self.render_device(&theme, cx),
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
                            .when_some(notice_block, |this, n| this.child(n))
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
