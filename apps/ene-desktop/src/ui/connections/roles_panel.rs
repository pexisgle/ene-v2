use crate::ui::icons::Icon;
use crate::ui::theme::Theme;
use crate::ui::widgets::{DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button};
use crate::ui::{Hub, ModelRoleSlot, SettingsStudio};
use ene_client_core::{
    connections::{decode_model_choice, encode_model_choice},
    domain::{Companion, ConnectionConfig, ConnectionKind, ModelAssignments},
};
use gpui::{AnyElement, Context, Entity, FontWeight, div, prelude::*, px};
use std::collections::BTreeMap;

type VoiceAssignFn = fn(&mut Hub, Option<(String, String)>, &mut Context<Hub>);

struct RolesContext {
    refresh_ids: Vec<String>,
    checking_any: bool,
    stt_models: Vec<(String, String, String)>,
    active_stt: Option<(String, String)>,
    tts_voices: Vec<(String, String, String)>,
    active_tts: Option<(String, String)>,
    conn_names: BTreeMap<String, String>,
    search_conns: Vec<(String, String, String)>,
    active_search_id: Option<String>,
}

struct VoiceRoleSpec {
    title: &'static str,
    dd_key: &'static str,
    filter_key: &'static str,
    filter_placeholder: &'static str,
    pick_placeholder: &'static str,
    off_label: &'static str,
    empty_dropdown_label: &'static str,
    empty_hint: &'static str,
    add_btn_id: &'static str,
    choices: Vec<(String, String, String)>,
    active: Option<(String, String)>,
    assign: VoiceAssignFn,
}

pub(super) struct ModelRoleChoices<'a> {
    pub(super) all_models: &'a [(String, String, String)],
    pub(super) embedding_models: &'a [(String, String, String)],
}

impl SettingsStudio {
    pub(super) fn render_advanced_roles_panel(
        &mut self,
        companion: Option<&Companion>,
        choices: ModelRoleChoices<'_>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(comp) = companion else {
            return render_no_companion_roles_hint(theme);
        };
        let assignments = comp.assignments.clone().unwrap_or_default();
        let roles_ctx = collect_roles_context(self.hub.read(cx));

        let mut rows: Vec<AnyElement> = ModelRoleSlot::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, slot)| {
                let slot_models = if slot == ModelRoleSlot::Embedding {
                    choices.embedding_models
                } else {
                    choices.all_models
                };
                self.render_model_role_row(
                    ModelRoleRowCtx {
                        idx,
                        slot,
                        assignments: &assignments,
                        slot_models,
                        live,
                    },
                    theme,
                    cx,
                )
            })
            .collect();

        for spec in voice_role_specs(&roles_ctx) {
            rows.push(self.render_voice_role_row(spec, &roles_ctx.conn_names, live, theme, cx));
        }
        rows.push(self.render_search_role_row(
            &roles_ctx.search_conns,
            roles_ctx.active_search_id.as_ref(),
            live,
            theme,
            cx,
        ));

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(render_roles_refresh_bar(
                &roles_ctx.refresh_ids,
                roles_ctx.checking_any,
                live,
                &self.hub,
                theme,
            ))
            .children(rows)
            .into_any_element()
    }
}

struct ModelRoleRowCtx<'a> {
    idx: usize,
    slot: ModelRoleSlot,
    assignments: &'a ModelAssignments,
    slot_models: &'a [(String, String, String)],
    live: bool,
}

impl SettingsStudio {
    fn render_model_role_row(
        &mut self,
        row: ModelRoleRowCtx<'_>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ModelRoleRowCtx {
            idx,
            slot,
            assignments,
            slot_models,
            live,
        } = row;
        let current = slot.current(assignments);
        let current_text = current
            .as_ref()
            .map(|m| {
                slot_models
                    .iter()
                    .find(|(cid, mid, _)| cid == &m.connection_id && mid == &m.model_id)
                    .map(|(_, _, label)| label.clone())
                    .unwrap_or_else(|| format!("{} / {}", m.connection_id, m.model_id))
            })
            .unwrap_or_else(|| slot.fallback_hint().to_owned());

        let dd_key = format!("adv-role:{slot:?}");
        let dd_open = self.open_dropdown.as_deref() == Some(dd_key.as_str());
        let filter_d = self.draft(
            cx,
            format!("adv-role:{slot:?}:filter"),
            "モデル名で絞り込み…",
            None,
            theme,
        );
        let filter_txt = filter_d.read(cx).text().to_owned();
        let options = build_slot_model_options(slot, slot_models);
        let current_value = current
            .as_ref()
            .map(|m| encode_model_choice(&m.connection_id, &m.model_id))
            .or_else(|| {
                (!matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task))
                    .then(|| "__auto__".to_owned())
            });
        let studio_toggle = cx.entity();
        let studio_pick = cx.entity();
        let hub_assign = self.hub.clone();
        let toggle_key = dd_key.clone();

        div()
            .when(idx > 0, |this| {
                this.border_t_1().border_color(theme.border).pt(px(12.))
            })
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(render_role_row_header(slot.label(), &current_text, theme))
            .child(dropdown_select(
                DropdownSpec {
                    id: dd_key,
                    selected_value: current_value.as_deref(),
                    selected_label: Some(current_text.as_str()),
                    placeholder: "モデルを選択…",
                    options: &options,
                    open: dd_open,
                    enabled: live,
                    max_list_height: 200.,
                    filter_draft: Some(filter_d),
                    filter_text: &filter_txt,
                    empty_label: "利用可能なモデルがありません（接続を追加・確認してください）",
                },
                theme,
                move |_, cx| toggle_studio_dropdown(&studio_toggle, &toggle_key, cx),
                move |picked, _, cx| {
                    if picked == "__auto__" {
                        hub_assign.update(cx, |hub, cx| hub.assign_model(slot, None, cx));
                    } else if let Some((cid, mid)) = decode_model_choice(&picked) {
                        hub_assign
                            .update(cx, |hub, cx| hub.assign_model(slot, Some((cid, mid)), cx));
                    }
                    close_studio_dropdown(&studio_pick, cx);
                },
            ))
            .into_any_element()
    }

    fn render_voice_role_row(
        &mut self,
        spec: VoiceRoleSpec,
        conn_names: &BTreeMap<String, String>,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current_text = resolve_voice_label(&spec, conn_names);
        let dd_open = self.open_dropdown.as_deref() == Some(spec.dd_key);
        let filter_d = self.draft(
            cx,
            spec.filter_key.to_owned(),
            spec.filter_placeholder,
            None,
            theme,
        );
        let filter_txt = filter_d.read(cx).text().to_owned();
        let options = build_voice_role_options(&spec);
        let current_value = spec
            .active
            .as_ref()
            .map(|(cid, vid)| encode_model_choice(cid, vid))
            .unwrap_or_else(|| "__off__".to_owned());
        let no_choices = spec.choices.is_empty();
        let assign_fn = spec.assign;
        let studio_toggle = cx.entity();
        let studio_pick = cx.entity();
        let hub_assign = self.hub.clone();
        let toggle_key = spec.dd_key.to_owned();

        div()
            .border_t_1()
            .border_color(theme.border)
            .pt(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(render_role_row_header(spec.title, &current_text, theme))
            .child(dropdown_select(
                DropdownSpec {
                    id: spec.dd_key.to_owned(),
                    selected_value: Some(current_value.as_str()),
                    selected_label: Some(current_text.as_str()),
                    placeholder: spec.pick_placeholder,
                    options: &options,
                    open: dd_open,
                    enabled: live,
                    max_list_height: 200.,
                    filter_draft: Some(filter_d),
                    filter_text: &filter_txt,
                    empty_label: spec.empty_dropdown_label,
                },
                theme,
                move |_, cx| toggle_studio_dropdown(&studio_toggle, &toggle_key, cx),
                move |picked, _, cx| {
                    if picked == "__off__" {
                        hub_assign.update(cx, |hub, cx| assign_fn(hub, None, cx));
                    } else if let Some((cid, vid)) = decode_model_choice(&picked) {
                        hub_assign.update(cx, |hub, cx| assign_fn(hub, Some((cid, vid)), cx));
                    }
                    close_studio_dropdown(&studio_pick, cx);
                },
            ))
            .when(no_choices, |this| {
                this.child(render_missing_role_hint(
                    spec.empty_hint,
                    spec.add_btn_id,
                    theme,
                    cx,
                ))
            })
            .into_any_element()
    }

    fn render_search_role_row(
        &self,
        search_conns: &[(String, String, String)],
        active_search_id: Option<&String>,
        live: bool,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let dd_key = "adv-role:search";
        let dd_open = self.open_dropdown.as_deref() == Some(dd_key);
        let mut options = Vec::with_capacity(search_conns.len() + 1);
        options.push(DropdownOption {
            value: "__off__".to_owned(),
            label: "無効（Web 検索オフ）".to_owned(),
            detail: None,
        });
        for (cid, name, adapter) in search_conns {
            options.push(DropdownOption {
                value: cid.clone(),
                label: name.clone(),
                detail: Some(adapter.clone()),
            });
        }
        let current_value = active_search_id
            .cloned()
            .unwrap_or_else(|| "__off__".to_owned());
        let current_text = active_search_id
            .and_then(|aid| {
                search_conns
                    .iter()
                    .find(|(cid, _, _)| cid == aid)
                    .map(|(_, name, adapter)| format!("{name} ({adapter})"))
            })
            .unwrap_or_else(|| "無効（Web 検索オフ）".to_owned());
        let studio_toggle = cx.entity();
        let studio_pick = cx.entity();
        let hub_assign = self.hub.clone();

        div()
            .border_t_1()
            .border_color(theme.border)
            .pt(px(12.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(render_role_row_header("Web 検索", &current_text, theme))
            .child(dropdown_select(
                DropdownSpec {
                    id: dd_key.to_owned(),
                    selected_value: Some(current_value.as_str()),
                    selected_label: Some(current_text.as_str()),
                    placeholder: "検索プロバイダを選択…",
                    options: &options,
                    open: dd_open,
                    enabled: live,
                    max_list_height: 180.,
                    filter_draft: None,
                    filter_text: "",
                    empty_label: "Web 検索接続がありません（「接続を追加」から追加してください）",
                },
                theme,
                move |_, cx| toggle_studio_dropdown(&studio_toggle, dd_key, cx),
                move |picked, _, cx| {
                    let target = (picked != "__off__").then_some(picked);
                    hub_assign.update(cx, |hub, cx| hub.assign_search_connection(target, cx));
                    close_studio_dropdown(&studio_pick, cx);
                },
            ))
            .when(search_conns.is_empty(), |this| {
                this.child(render_missing_role_hint(
                    "Web 検索接続がありません。未設定の間は外部検索を行いません。",
                    "adv-role-search-add",
                    theme,
                    cx,
                ))
            })
            .into_any_element()
    }
}

fn render_no_companion_roles_hint(theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(px(14.))
        .p(px(16.))
        .text_xs()
        .text_color(theme.ink_muted)
        .child("コンパニオンを選択すると、役割別のモデル割当を調整できます。")
        .into_any_element()
}

fn collect_roles_context(hub: &Hub) -> RolesContext {
    let settings = hub.model.settings();
    let refresh_ids = settings
        .map(|s| {
            s.connections
                .iter()
                .filter(|c| {
                    c.enabled
                        && matches!(
                            c.kind,
                            ConnectionKind::Llm
                                | ConnectionKind::Embedding
                                | ConnectionKind::Stt
                                | ConnectionKind::Tts
                        )
                })
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default();
    let conn_names = settings
        .map(|s| {
            s.connections
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let search_conns = settings
        .map(|s| {
            s.connections
                .iter()
                .filter(|c| c.kind == ConnectionKind::Search)
                .map(|c| {
                    let adapter = c
                        .config
                        .as_ref()
                        .and_then(|cfg| match cfg {
                            ConnectionConfig::Search(sc) => {
                                sc.endpoint.as_ref().map(|e| e.adapter.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| "search".into());
                    (c.id.clone(), c.name.clone(), adapter)
                })
                .collect()
        })
        .unwrap_or_default();
    let active_search_id = settings.and_then(|s| {
        s.connections
            .iter()
            .find(|c| c.kind == ConnectionKind::Search && c.enabled)
            .map(|c| c.id.clone())
    });
    RolesContext {
        refresh_ids,
        checking_any: !hub.checking_connections.is_empty(),
        stt_models: hub.all_stt_models(),
        active_stt: hub.active_stt_selection(),
        tts_voices: hub.all_tts_voices(),
        active_tts: hub.active_tts_selection(),
        conn_names,
        search_conns,
        active_search_id,
    }
}

fn voice_role_specs(ctx: &RolesContext) -> [VoiceRoleSpec; 2] {
    [
        VoiceRoleSpec {
            title: "音声認識（STT）",
            dd_key: "adv-role:stt",
            filter_key: "adv-role:stt:filter",
            filter_placeholder: "モデル名で絞り込み…",
            pick_placeholder: "モデルを選択…",
            off_label: "無効（音声入力オフ）",
            empty_dropdown_label: "利用可能な音声認識モデルがありません（「接続を追加」から追加してください）",
            empty_hint: "音声認識（STT）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-stt-add",
            choices: ctx.stt_models.clone(),
            active: ctx.active_stt.clone(),
            assign: Hub::assign_stt_model,
        },
        VoiceRoleSpec {
            title: "音声合成（TTS）",
            dd_key: "adv-role:tts",
            filter_key: "adv-role:tts:filter",
            filter_placeholder: "話者名・接続名で絞り込み…",
            pick_placeholder: "話者を選択…",
            off_label: "無効（読み上げオフ）",
            empty_dropdown_label: "利用可能な音声合成話者がありません（「接続を追加」から追加してください）",
            empty_hint: "音声合成（TTS）接続がありません。「接続を追加」から追加してください。",
            add_btn_id: "adv-role-tts-add",
            choices: ctx.tts_voices.clone(),
            active: ctx.active_tts.clone(),
            assign: Hub::assign_tts_voice,
        },
    ]
}

fn render_roles_refresh_bar(
    refresh_ids: &[String],
    checking_any: bool,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_refresh_all = hub.clone();
    let ids = refresh_ids.to_vec();
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
                .child("役割ごとに使用するモデルをドロップダウンから割り当てます。"),
        )
        .when(!ids.is_empty(), |this| {
            this.child(pill_button(
                "adv-roles-refresh-all",
                if checking_any {
                    "モデル一覧を取得中…"
                } else {
                    "全接続のモデルを再取得"
                },
                Tone::Ghost
                    .icon(Icon::Refresh)
                    .enabled(live && !checking_any),
                theme,
                move |_, cx| {
                    hub_refresh_all.update(cx, |hub, cx| {
                        for cid in &ids {
                            hub.check_connection(cid.clone(), cx);
                        }
                    });
                },
            ))
        })
}

fn build_slot_model_options(
    slot: ModelRoleSlot,
    slot_models: &[(String, String, String)],
) -> Vec<DropdownOption> {
    let mut options = Vec::new();
    if !matches!(slot, ModelRoleSlot::Companion | ModelRoleSlot::Task) {
        options.push(DropdownOption {
            value: "__auto__".to_owned(),
            label: slot.fallback_hint().to_owned(),
            detail: None,
        });
    }
    for (cid, mid, label) in slot_models {
        options.push(DropdownOption {
            value: encode_model_choice(cid, mid),
            label: label.clone(),
            detail: Some(mid.clone()),
        });
    }
    options
}

fn resolve_voice_label(spec: &VoiceRoleSpec, conn_names: &BTreeMap<String, String>) -> String {
    spec.active
        .as_ref()
        .map(|(cid, vid)| {
            spec.choices
                .iter()
                .find(|(c, v, _)| c == cid && v == vid)
                .map(|(_, _, label)| label.clone())
                .unwrap_or_else(|| {
                    let cname = conn_names
                        .get(cid)
                        .map(String::as_str)
                        .unwrap_or(cid.as_str());
                    format!("{cname} / {vid}")
                })
        })
        .unwrap_or_else(|| spec.off_label.to_owned())
}

fn build_voice_role_options(spec: &VoiceRoleSpec) -> Vec<DropdownOption> {
    let mut options = Vec::with_capacity(spec.choices.len() + 1);
    options.push(DropdownOption {
        value: "__off__".to_owned(),
        label: spec.off_label.to_owned(),
        detail: None,
    });
    for (cid, vid, label) in &spec.choices {
        options.push(DropdownOption {
            value: encode_model_choice(cid, vid),
            label: label.clone(),
            detail: Some(vid.clone()),
        });
    }
    options
}

fn render_role_row_header(title: &str, current_text: &str, theme: &Theme) -> gpui::Div {
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
                .text_color(theme.ink)
                .child(title.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .truncate()
                .child(current_text.to_owned()),
        )
}

fn render_missing_role_hint(
    hint: &'static str,
    btn_id: &'static str,
    theme: &Theme,
    cx: &Context<SettingsStudio>,
) -> gpui::Div {
    let studio_add = cx.entity();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(div().text_xs().text_color(theme.ink_muted).child(hint))
        .child(pill_button(
            btn_id,
            "接続を追加を開く",
            Tone::Ghost.icon(Icon::Plus),
            theme,
            move |_, cx| {
                studio_add.update(cx, |this, cx| {
                    this.connection_form.adding_connection = true;
                    cx.notify();
                });
            },
        ))
}

fn toggle_studio_dropdown(studio: &Entity<SettingsStudio>, key: &str, cx: &mut gpui::App) {
    studio.update(cx, |this, cx| {
        this.open_dropdown = if this.open_dropdown.as_deref() == Some(key) {
            None
        } else {
            Some(key.to_owned())
        };
        cx.notify();
    });
}

fn close_studio_dropdown(studio: &Entity<SettingsStudio>, cx: &mut gpui::App) {
    studio.update(cx, |this, cx| {
        this.open_dropdown = None;
        cx.notify();
    });
}
