//! The 62px bordered companion row: badge, persona line, status actions and
//! the inline editor disclosed beneath it.

use super::{CompanionsPane, CompanionsSnapshot, editor};
use crate::ui::theme::Theme;
use crate::ui::widgets::{ButtonStyle, Tone, companion_badge, pill_button, status_pill};
use ene_client_core::domain::Companion;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

struct RowStatus {
    active: bool,
    resident: bool,
    expanded: bool,
    live: bool,
}

impl CompanionsPane {
    pub(super) fn render_companion_row(
        &mut self,
        comp: &Companion,
        snap: &CompanionsSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = comp.id.clone();
        let active = snap.active_id.as_deref() == Some(id.as_str());
        let resident = snap.resident_ids.iter().any(|rid| rid == &id);
        let expanded = self.form.expanded_companion_id.as_deref() == Some(id.as_str());
        let row_header = self.render_companion_row_header(
            comp,
            &RowStatus {
                active,
                resident,
                expanded,
                live: snap.live,
            },
            theme,
            cx,
        );

        let exported_for_id = snap
            .exported_template
            .as_ref()
            .filter(|(cid, _)| cid == &id)
            .map(|(_, json)| json.as_str());
        let editor = expanded.then(|| {
            editor::render_companion_editor(
                self,
                &editor::EditorContext {
                    comp,
                    mind_view: snap.minds.get(&id),
                    tts_voices: &snap.tts_voices,
                    tts_conn_ids: &snap.tts_conn_ids,
                    exported_json: exported_for_id,
                    live: snap.live,
                },
                theme,
                cx,
            )
        });

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(crate::ui::theme::radius::surface())
            .flex()
            .flex_col()
            .child(row_header)
            .when_some(editor, gpui::ParentElement::child)
            .into_any_element()
    }

    fn render_companion_row_header(
        &self,
        comp: &Companion,
        status: &RowStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let persona_line = comp
            .persona
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("話し方未設定")
            .to_owned();
        let identity = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.))
            .child(companion_badge(
                &comp.name,
                &comp.id,
                34.,
                (status.active || status.resident).then_some(theme.mint),
                theme,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .truncate()
                            .child(comp.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .truncate()
                            .child(persona_line),
                    ),
            );
        let actions = self.render_companion_row_actions(&comp.id, status, theme, cx);

        div()
            .h(px(62.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .child(identity)
            .child(actions)
            .into_any_element()
    }

    fn render_companion_row_actions(
        &self,
        id: &str,
        status: &RowStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let RowStatus {
            active,
            resident,
            expanded,
            live,
        } = *status;
        let hub_select = self.hub.clone();
        let select_id = id.to_owned();
        let hub_res = self.hub.clone();
        let res_id = id.to_owned();
        let studio_expand = cx.entity();
        let expand_id = id.to_owned();
        let hub_expand = self.hub.clone();

        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.))
            .child(pill_button(
                SharedString::from(format!("comp-res-{id}")),
                if resident { "常駐中" } else { "常駐" },
                ButtonStyle::new(if resident { Tone::Mint } else { Tone::Ghost }).enabled(live),
                theme,
                move |_, cx| {
                    hub_res.update(cx, |hub, cx| {
                        if resident {
                            hub.release_companion(res_id.clone(), cx);
                        } else {
                            hub.activate_companion(res_id.clone(), cx);
                        }
                    });
                },
            ))
            .child(if active {
                status_pill("会話中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
            } else {
                pill_button(
                    SharedString::from(format!("comp-select-{id}")),
                    "選択",
                    Tone::Soft.enabled(live),
                    theme,
                    move |_, cx| {
                        hub_select.update(cx, |hub, cx| {
                            hub.select_companion(&select_id, cx);
                        });
                    },
                )
            })
            .child(pill_button(
                SharedString::from(format!("comp-edit-{id}")),
                if expanded { "閉じる" } else { "編集" },
                ButtonStyle::new(if expanded { Tone::Soft } else { Tone::Ghost }),
                theme,
                move |_, cx| {
                    let mut next_open = None;
                    studio_expand.update(cx, |this, cx| {
                        if this.form.expanded_companion_id.as_deref() == Some(expand_id.as_str()) {
                            this.form.expanded_companion_id = None;
                        } else {
                            this.form.expanded_companion_id = Some(expand_id.clone());
                            next_open = Some(expand_id.clone());
                        }
                        this.form.delete_armed_companion = None;
                        cx.notify();
                    });
                    if let Some(target_id) = next_open {
                        hub_expand.update(cx, |hub, _| {
                            hub.load_mind_for(target_id);
                        });
                    }
                },
            ))
            .into_any_element()
    }
}
