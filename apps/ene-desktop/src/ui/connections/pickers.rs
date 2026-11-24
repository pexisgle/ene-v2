use super::ConnectionsPane;
use super::kind_pickers;
use super::row_editor::is_voicevox_tts;
use crate::input::Draft;
use crate::ui::Hub;
use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{
    DropdownOption, DropdownSpec, Tone, dropdown_select, kind_pill, pill_button,
};
use ene_client_core::domain::{Connection, ConnectionEndpoint, ConnectionKind};
use gpui::{Context, Entity, FontWeight, SharedString, div, prelude::*, px};
use std::rc::Rc;

pub(super) struct PickerInput<'a> {
    pub(super) conn: &'a Connection,
    pub(super) endpoint: Option<&'a ConnectionEndpoint>,
    pub(super) list_d: &'a Entity<Draft>,
    pub(super) dim_d: Option<&'a Entity<Draft>>,
    pub(super) live: bool,
}

pub(super) struct ResourcePickerSpec<'a> {
    pub(super) dd_key: String,
    pub(super) fetch_btn_id: String,
    pub(super) title: &'static str,
    pub(super) badge_label: Option<String>,
    pub(super) fetch_label: &'static str,
    pub(super) placeholder: &'static str,
    pub(super) empty_label: &'static str,
    pub(super) hint: Option<&'static str>,
    pub(super) list_height: f32,
    pub(super) current_selected: Option<&'a str>,
    pub(super) selected_label: Option<&'a str>,
    pub(super) options: &'a [DropdownOption],
    pub(super) filter_d: Entity<Draft>,
    pub(super) filter_txt: &'a str,
    pub(super) is_checking: bool,
    pub(super) live: bool,
}

pub(super) fn render_kind_dropdown_picker(
    pane: &mut ConnectionsPane,
    input: PickerInput<'_>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Option<gpui::Div> {
    let id = input.conn.id.clone();
    let is_checking = pane.hub.read(cx).setup.checking_connections.contains(&id);
    let kind = input.conn.kind;
    if kind == ConnectionKind::Llm {
        return Some(kind_pickers::build_llm_picker(
            pane,
            &id,
            input.list_d,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if is_voicevox_tts(input.conn, input.endpoint) {
        return Some(kind_pickers::build_voicevox_picker(
            pane,
            (&id, input.endpoint, input.list_d),
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Tts && input.conn.is_sakura() {
        return Some(kind_pickers::build_sakura_tts_picker(
            pane,
            &id,
            input.list_d,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Stt {
        return Some(kind_pickers::build_stt_picker(
            pane,
            &id,
            input.endpoint,
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    if kind == ConnectionKind::Embedding {
        return Some(kind_pickers::build_embedding_picker(
            pane,
            &id,
            (input.list_d, input.dim_d),
            (is_checking, input.live),
            theme,
            cx,
        ));
    }
    None
}
pub(super) fn render_resource_picker<F>(
    pane: &ConnectionsPane,
    spec: ResourcePickerSpec<'_>,
    conn_id: &str,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
    on_select: F,
) -> gpui::Div
where
    F: Fn(&mut Hub, String, &mut Context<Hub>) + 'static,
{
    let dd_open = pane.open_dropdown.as_deref() == Some(spec.dd_key.as_str());
    let pane_toggle = cx.entity();
    let pane_pick = cx.entity();
    let hub_refresh = pane.hub.clone();
    let hub_select = pane.hub.clone();
    let refresh_id = conn_id.to_owned();
    let toggle_key = spec.dd_key.clone();
    let on_select = Rc::new(on_select);

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink_muted)
                                .child(spec.title),
                        )
                        .when_some(spec.badge_label, |this, badge| {
                            this.child(kind_pill(badge, theme))
                        }),
                )
                .child(pill_button(
                    SharedString::from(spec.fetch_btn_id),
                    spec.fetch_label,
                    Tone::Ghost
                        .icon(Icon::Refresh)
                        .enabled(spec.live && !spec.is_checking),
                    theme,
                    move |_, cx| {
                        hub_refresh
                            .update(cx, |hub, cx| hub.check_connection(refresh_id.clone(), cx));
                    },
                )),
        )
        .child(dropdown_select(
            DropdownSpec {
                id: spec.dd_key,
                selected_value: spec.current_selected,
                selected_label: spec.selected_label,
                placeholder: spec.placeholder,
                options: spec.options,
                open: dd_open,
                enabled: spec.live,
                max_list_height: spec.list_height,
                filter_draft: Some(spec.filter_d),
                filter_text: spec.filter_txt,
                empty_label: spec.empty_label,
            },
            theme,
            move |_, cx| {
                pane_toggle.update(cx, |this, cx| {
                    this.open_dropdown =
                        if this.open_dropdown.as_deref() == Some(toggle_key.as_str()) {
                            None
                        } else {
                            Some(toggle_key.clone())
                        };
                    cx.notify();
                });
            },
            move |picked, _, cx| {
                let cb = Rc::clone(&on_select);
                hub_select.update(cx, move |hub, cx| cb(hub, picked, cx));
                pane_pick.update(cx, |this, cx| {
                    this.open_dropdown = None;
                    cx.notify();
                });
            },
        ))
        .when_some(spec.hint, |this, hint| {
            this.child(div().text_xs().text_color(theme.ink_muted).child(hint))
        })
}
