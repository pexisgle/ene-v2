//! Companions chapter in `SettingsStudio`: 3-field quick creation, 62px bordered rows,
//! 1-level hairline-divided editor, and collapsed `MindProfile` disclosure.

mod create;
mod editor;
pub(crate) mod voice;

use super::icons::Icon;
use super::theme::Theme;
use super::widgets::{ButtonStyle, Tone, companion_badge, pill_button, status_pill};
use super::{DraftStore, Hub};
use ene_client_core::domain::{Companion, CompanionTemplate, ConnectionKind, MindView};
use gpui::{
    AnyElement, Context, Entity, FontWeight, SharedString, Subscription, div, prelude::*, px,
};
use std::collections::BTreeMap;

/// Form and disclosure state owned by [`CompanionsPane`].
#[derive(Default)]
pub(super) struct CompanionFormState {
    pub(super) creating_companion: bool,
    pub(super) importing_template: bool,
    pub(super) preview_pending: bool,
    pub(super) expanded_companion_id: Option<String>,
    pub(super) mind_disclosure_open: bool,
    pub(super) delete_armed_companion: Option<String>,
    last_created_ping: u64,
}

/// Companion management chapter rendered as an independent GPUI pane entity.
pub(crate) struct CompanionsPane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    pub(super) form: CompanionFormState,
    pub(super) open_dropdown: Option<String>,
    initialized: bool,
    _observe: Subscription,
}

impl std::fmt::Debug for CompanionsPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionsPane")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

struct RowStatus {
    active: bool,
    resident: bool,
    expanded: bool,
    live: bool,
}

pub(super) struct CompanionsSnapshot {
    pub(super) live: bool,
    pub(super) companions: Vec<Companion>,
    pub(super) active_id: Option<String>,
    pub(super) resident_ids: Vec<String>,
    pub(super) has_models: bool,
    pub(super) tts_voices: Vec<(String, String, String, String)>,
    pub(super) tts_conn_ids: Vec<String>,
    pub(super) minds: BTreeMap<String, MindView>,
    pub(super) templates: Vec<CompanionTemplate>,
    pub(super) exported_template: Option<(String, String)>,
    pub(super) preview_reply: Option<Result<String, ene_client_core::state::PreviewError>>,
}

impl CompanionsSnapshot {
    fn capture(hub: &Hub) -> Self {
        let tts_conn_ids: Vec<String> = hub
            .model
            .settings()
            .map(|s| {
                s.connections
                    .iter()
                    .filter(|c| c.kind == ConnectionKind::Tts && c.enabled)
                    .map(|c| c.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            live: hub.live(),
            companions: hub
                .model
                .snapshot()
                .map(|s| s.companions.clone())
                .unwrap_or_default(),
            active_id: hub.model.companion_id().map(str::to_owned),
            resident_ids: hub.model.resident_companions(&hub.options.id),
            has_models: !hub.model_choices().is_empty(),
            tts_voices: hub.tts_voice_choices(),
            tts_conn_ids,
            minds: hub.model.companions.minds.clone(),
            templates: hub.model.companions.templates.clone(),
            exported_template: hub.model.companions.exported_template.clone(),
            preview_reply: hub.model.companions.preview_reply.clone(),
        }
    }
}

impl CompanionsPane {
    pub(crate) fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            drafts: DraftStore::default(),
            form: CompanionFormState::default(),
            open_dropdown: None,
            initialized: false,
            _observe,
        }
    }

    pub(crate) fn on_leave(&mut self) {
        self.open_dropdown = None;
    }

    fn sync_hub_events(&mut self, cx: &mut Context<Self>) {
        let (created_ping, active_comp, has_preview) = {
            let hub = self.hub.read(cx);
            (
                hub.setup.created_ping,
                hub.model.companion_id().map(str::to_owned),
                hub.model.companions.preview_reply.is_some(),
            )
        };
        if has_preview {
            self.form.preview_pending = false;
        }
        if !self.initialized {
            self.initialized = true;
            self.form.expanded_companion_id = active_comp;
            self.form.last_created_ping = created_ping;
            return;
        }
        if created_ping != self.form.last_created_ping {
            self.form.last_created_ping = created_ping;
            self.form.creating_companion = false;
            self.form.importing_template = false;
            self.form.preview_pending = false;
            self.form.expanded_companion_id = active_comp;
            self.drafts
                .clear_keys(cx, ["new:name", "new:mind", "new:persona", "import:json"]);
        }
    }

    fn render_companion_row(
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
            RowStatus {
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
                editor::EditorContext {
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
            .when_some(editor, |this, ed| this.child(ed))
            .into_any_element()
    }

    fn render_companion_row_header(
        &self,
        comp: &Companion,
        status: RowStatus,
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
        status: RowStatus,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let RowStatus {
            active,
            resident,
            expanded,
            live,
        } = status;
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

impl gpui::Render for CompanionsPane {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_hub_events(cx);
        let theme = self.hub.read(cx).theme();
        let snap = CompanionsSnapshot::capture(self.hub.read(cx));
        let creating = self.form.creating_companion;
        let importing = self.form.importing_template;
        let header = render_companions_header(snap.live, creating, importing, &theme, cx);

        let import_panel = importing
            .then(|| create::render_import_panel(self, snap.live, &snap.templates, &theme, cx));
        let create_panel = creating.then(|| create::render_create_panel(self, &snap, &theme, cx));

        let mut rows: Vec<AnyElement> = snap
            .companions
            .iter()
            .map(|comp| self.render_companion_row(comp, &snap, &theme, cx))
            .collect();

        if rows.is_empty() {
            rows.push(render_empty_companions_row(creating, &theme, cx));
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(import_panel, |this, p| this.child(p))
            .when_some(create_panel, |this, p| this.child(p))
            .child(div().w_full().flex().flex_col().gap(px(10.)).children(rows))
    }
}

fn render_companions_header(
    live: bool,
    creating: bool,
    importing: bool,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let pane_toggle_create = cx.entity();
    let pane_toggle_import = cx.entity();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .child(
            div()
                .text_size(px(22.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("コンパニオン"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(pill_button(
                    "comp-toggle-import",
                    if importing {
                        "読込を閉じる"
                    } else {
                        "テンプレート読込"
                    },
                    Tone::Ghost.enabled(live),
                    theme,
                    move |_, cx| {
                        pane_toggle_import.update(cx, |this, cx| {
                            this.form.importing_template = !this.form.importing_template;
                            if this.form.importing_template {
                                this.form.creating_companion = false;
                            }
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    "comp-toggle-create",
                    if creating {
                        "閉じる"
                    } else {
                        "新規作成"
                    },
                    ButtonStyle::new(if creating { Tone::Ghost } else { Tone::Primary })
                        .maybe_icon((!creating).then_some(Icon::Plus)),
                    theme,
                    move |_, cx| {
                        pane_toggle_create.update(cx, |this, cx| {
                            this.form.creating_companion = !this.form.creating_companion;
                            if this.form.creating_companion {
                                this.form.importing_template = false;
                            }
                            cx.notify();
                        });
                    },
                )),
        )
        .into_any_element()
}

fn render_empty_companions_row(
    creating: bool,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .px(px(16.))
        .py(px(18.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("登録されているコンパニオンはありません。"),
        )
        .when(!creating, |this| {
            let pane = cx.entity();
            this.child(pill_button(
                "comp-empty-create",
                "新規作成",
                Tone::Primary.icon(Icon::Plus),
                theme,
                move |_, cx| {
                    pane.update(cx, |this, cx| {
                        this.form.creating_companion = true;
                        cx.notify();
                    });
                },
            ))
        })
        .into_any_element()
}

pub(super) fn parse_voice_selection(
    raw_key: &str,
    manual_fallback: &str,
) -> (Option<String>, Option<String>) {
    let trimmed = raw_key.trim();
    if let Some((cid, vid)) = trimmed.split_once('\0') {
        let conn = (!cid.trim().is_empty()).then(|| cid.trim().to_owned());
        let voice = (!vid.trim().is_empty()).then(|| vid.trim().to_owned());
        return (conn, voice);
    }
    let fallback = manual_fallback.trim();
    if fallback.is_empty() {
        (None, None)
    } else {
        (None, Some(fallback.to_owned()))
    }
}

pub(super) fn split_pipe(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b11_parse_voice_selection_preserves_connection_and_duplicate_voice_id() {
        assert_eq!(
            parse_voice_selection("tts-b\0shared-id", ""),
            (Some("tts-b".into()), Some("shared-id".into()))
        );
        assert_eq!(
            parse_voice_selection("", "manual-3"),
            (None, Some("manual-3".into()))
        );
        assert_eq!(parse_voice_selection("", ""), (None, None));
    }
}
