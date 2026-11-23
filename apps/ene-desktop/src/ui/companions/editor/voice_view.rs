//! The voice selector and its header row.
//!
//! Split out of `views.rs` because they are self-contained: they read a
//! draft and a snapshot and draw, with no knowledge of the rest of the
//! editor.

//! The companion editor's visual sections.

//! Per-companion profile, VRM model, voice binding, and mind profile editor.

use super::super::CompanionsPane;
use crate::ui::{
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{DropdownSpec, Tone, dropdown_select, pill_button},
};
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

use super::form::*;

pub(super) fn render_voice_selector(
    pane: &mut CompanionsPane,
    ctx: &EditorContext<'_>,
    drafts: &CompanionEditorDrafts,
    voice_selection: (&str, Option<&str>, Option<&str>),
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let id = &ctx.comp.id;
    let (voice_key_val, picked_conn_id, picked_voice_id) = voice_selection;
    let voice_dd_key = format!("comp-voice:{id}");
    let voice_dd_open = pane.open_dropdown.as_deref() == Some(voice_dd_key.as_str());
    let voice_filter_d = pane.drafts.draft(
        cx,
        format!("comp:{id}:voice-filter"),
        "話者名・IDで絞り込み…",
        None,
        theme,
    );
    let voice_filter_txt = voice_filter_d.read(cx).text().to_owned();
    let voice_options = build_voice_options(ctx.tts_voices);
    let selected_voice_label =
        format_selected_voice_label(ctx.tts_voices, picked_conn_id, picked_voice_id);
    let selected_dropdown_val = if !voice_key_val.is_empty() {
        Some(voice_key_val)
    } else {
        Some("")
    };

    let pane_voice_toggle = cx.entity();
    let pane_voice_pick = cx.entity();
    let toggle_key = voice_dd_key.clone();
    let voice_key_d_pick = drafts.voice_key_d.clone();
    let voice_d_pick = drafts.voice_d.clone();
    let voice_checking = {
        let hub = pane.hub.read(cx);
        ctx.tts_conn_ids
            .iter()
            .any(|cid| hub.setup.checking_connections.contains(cid))
    };

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(render_voice_header_row(
            id,
            voice_checking,
            ctx,
            &pane.hub,
            theme,
        ))
        .child(dropdown_select(
            DropdownSpec {
                id: voice_dd_key,
                selected_value: selected_dropdown_val,
                selected_label: selected_voice_label.as_deref(),
                placeholder: "接続の既定を使用",
                options: &voice_options,
                open: voice_dd_open,
                enabled: true,
                max_list_height: 210.,
                filter_draft: Some(voice_filter_d),
                filter_text: &voice_filter_txt,
                empty_label: "有効なTTS接続から話者を取得できませんでした",
            },
            theme,
            move |_, cx| {
                pane_voice_toggle.update(cx, |this, cx| {
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
                let raw_vid = picked
                    .split_once('\0')
                    .map_or_else(|| picked.clone(), |(_, v)| v.to_owned());
                voice_key_d_pick.update(cx, |d, cx| d.set(picked, cx));
                voice_d_pick.update(cx, |d, cx| d.set(raw_vid, cx));
                pane_voice_pick.update(cx, |this, cx| {
                    this.open_dropdown = None;
                    cx.notify();
                });
            },
        ))
        .when(ctx.tts_voices.is_empty(), |this| {
            this.child(drafts.voice_d.clone())
        })
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(div().flex_1().child(labeled_field(
                    "話速",
                    drafts.voice_speed_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "音高",
                    drafts.voice_pitch_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "調子",
                    drafts.voice_style_d.clone(),
                    theme,
                ))),
        )
        .into_any_element()
}

pub(super) fn render_voice_header_row(
    id: &str,
    voice_checking: bool,
    ctx: &EditorContext<'_>,
    hub: &Entity<crate::ui::Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_refresh_voice = hub.clone();
    let refresh_tts_ids = ctx.tts_conn_ids.to_vec();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child("声（接続・話者）"),
        )
        .when(!refresh_tts_ids.is_empty(), |this| {
            this.child(pill_button(
                SharedString::from(format!("comp-voice-refresh-{id}")),
                if voice_checking {
                    "話者を取得中…"
                } else {
                    "話者一覧を取得"
                },
                Tone::Ghost
                    .icon(Icon::Refresh)
                    .enabled(ctx.live && !voice_checking),
                theme,
                move |_, cx| {
                    hub_refresh_voice.update(cx, |hub, cx| {
                        for cid in &refresh_tts_ids {
                            hub.check_connection(cid.clone(), cx);
                        }
                    });
                },
            ))
        })
}
