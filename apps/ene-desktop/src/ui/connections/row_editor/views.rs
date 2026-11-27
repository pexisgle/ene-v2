//! The connection editor's visual parts.

use super::ConnectionsPane;
use crate::ui::connections::EditorMode;
use crate::ui::connections::mcp_editor::{append_mcp_extra_fields, render_mcp_tools_strip};
use crate::ui::connections::pickers::{PickerInput, render_resource_picker};
use crate::ui::connections::schema_form::{render_agent_platform_options, render_options_schema};
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, toggle_switch};
use crate::ui::{GOOGLE_AGENT_PLATFORM, Hub};
use ene_client_core::{
    connections::{ConnectionEditSpec, ConnectionResource, apply_connection_edits},
    domain::{Connection, ConnectionCheck, ConnectionKind},
};
use ene_domain::CredentialState;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

use super::form::*;

pub(super) fn render_connection_editor(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    check: Option<&ConnectionCheck>,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let id = conn.id.clone();
    let endpoint = conn.endpoint();
    let harness = conn.kind == ConnectionKind::Acp;
    let agent_platform = conn.kind == ConnectionKind::Llm
        && endpoint.is_some_and(|ep| ep.adapter == GOOGLE_AGENT_PLATFORM);

    let drafts = collect_editor_drafts(pane, conn, endpoint, theme, cx);
    let mcp_tools_strip = render_mcp_tools_strip(pane, conn, check, &drafts.list_d, theme, cx);
    let resource = ConnectionResource::for_connection(conn);
    let dropdown_picker = resource.map(|resource| {
        render_resource_picker(
            pane,
            &PickerInput {
                id: &conn.id,
                resource,
                endpoint,
                list_d: &drafts.list_d,
                dim_d: drafts.dim_d.as_ref(),
                live,
            },
            theme,
            cx,
        )
    });
    let (schema_controls, next_options_json) = if harness {
        (Vec::new(), Ok(None))
    } else if agent_platform {
        render_agent_platform_options(pane, &id, conn.revision, endpoint, theme, cx)
    } else {
        render_options_schema(pane, &id, conn.revision, endpoint, theme, cx)
    };

    let options_error = next_options_json.as_ref().err().map(ToString::to_string);
    let mut payload =
        build_editor_save_payload(&drafts, next_options_json.ok().flatten(), live, cx);
    let search_cap_invalid = conn.kind == ConnectionKind::Search
        && ene_client_core::connections::parse_search_cap(&payload.next_list).is_none();
    payload.can_save &= !search_cap_invalid && options_error.is_none();
    let args_error = payload.next_args.as_ref().err().copied();
    let mcp_kv_error = payload
        .next_mcp_extra
        .as_ref()
        .and_then(|extra| extra.as_ref().err().copied());
    let show_list_field = matches!(
        resource,
        None | Some(ConnectionResource::ChatModel | ConnectionResource::TranscriptionModel)
    );

    let cred_strip = (!harness).then(|| render_credentials_strip(pane, conn, live, theme, cx));
    let actions = render_editor_actions(conn, payload, &pane.hub, theme);

    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .when(conn.kind != ConnectionKind::Search, |this| {
            this.child(render_enable_row(conn, live, &pane.hub, theme))
        })
        .when_some(error_banner(check, theme), gpui::ParentElement::child)
        .when_some(cred_strip, gpui::ParentElement::child)
        .child(labeled_field("表示名", drafts.name_d, theme))
        .when(harness, |this| {
            let target = endpoint.map_or("未指定", |ep| ep.adapter.as_str());
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(format!("ハーネス ID　{target}")),
            )
        })
        .when_some(drafts.cmd_d, |this, cd| {
            this.child(labeled_field("コマンド", cd, theme))
        })
        .when_some(drafts.mcp_args_d, |this, draft| {
            append_mcp_args_field(this, &id, draft, args_error, theme)
        })
        .when_some(drafts.mcp_extra_d, |this, group| {
            append_mcp_extra_fields(this, &id, group, mcp_kv_error, theme)
        })
        .when(!agent_platform && !harness, |this| {
            this.child(labeled_field("エンドポイント URL", drafts.ep_d, theme))
        })
        .when_some(dropdown_picker, gpui::ParentElement::child)
        .when_some(drafts.dim_d, |this, dd| {
            this.child(labeled_field("次元数 (dimensions)", dd, theme))
        })
        .when(show_list_field && !harness, |this| {
            this.child(labeled_field(
                match conn.kind {
                    ConnectionKind::Search => "最大検索結果数（1–20、空欄で接続上限なし）",
                    ConnectionKind::Llm => "候補のモデル ID（カンマ区切り）",
                    _ => conn.kind.list_field_label(),
                },
                drafts.list_d,
                theme,
            ))
        })
        .when(search_cap_invalid, |this| {
            this.child(
                div()
                    .text_xs()
                    .text_color(theme.danger_fg)
                    .child("1から20までの整数を入力してください"),
            )
        })
        .when_some(mcp_tools_strip, gpui::ParentElement::child)
        .when(!schema_controls.is_empty(), |this| {
            this.child(render_schema_box(schema_controls, theme))
        })
        .when_some(options_error, |this, error| {
            this.child(div().text_xs().text_color(theme.danger_fg).child(error))
        })
        .child(actions)
        .into_any_element()
}

pub(super) fn render_credentials_strip(
    pane: &mut ConnectionsPane,
    conn: &Connection,
    live: bool,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> gpui::Div {
    let id = conn.id.clone();
    let editor = pane.form.editor(&id);
    let revealed = editor.is_some_and(|editor| editor.key_visible);
    let mode = editor.map_or_default(|editor| editor.mode);
    let secret_d = pane.drafts.masked_draft(
        cx,
        format!("conn:secret:{id}"),
        "新しいAPIキー / トークン",
        !revealed,
        theme,
    );
    let has_cred = conn.credential_state.is_configured();
    let cred_label = match conn.credential_state {
        CredentialState::Registered => "登録済み",
        CredentialState::Environment => "環境変数",
        CredentialState::Absent => "未登録",
        CredentialState::RegistrationRequired => "再登録が必要",
    };

    div()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(render_cred_strip_header(cred_label, has_cred, theme))
        .child(render_cred_secret_row(
            CredSecretInput {
                id: &id,
                secret_d,
                revealed,
                live,
            },
            &pane.hub,
            theme,
            cx,
        ))
        .when(
            conn.credential_state == CredentialState::Registered,
            |this| this.child(render_revoke_controls(&id, mode, live, theme, cx)),
        )
}

pub(super) fn render_schema_box(schema_controls: Vec<AnyElement>, theme: &Theme) -> gpui::Div {
    div()
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_blush)
        .p(px(12.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .children(schema_controls)
}

fn render_revoke_controls(
    id: &str,
    mode: EditorMode,
    live: bool,
    theme: &Theme,
    cx: &Context<ConnectionsPane>,
) -> gpui::Div {
    let pane = cx.entity();
    let cancel_id = id.to_owned();
    let confirming = mode == EditorMode::ConfirmRevoke;
    let cancel = confirming.then(|| {
        let pane = pane.clone();
        pill_button(
            SharedString::from(format!("conn-rev-cancel-{id}")),
            "キャンセル",
            Tone::Ghost,
            theme,
            move |_, cx| {
                pane.update(cx, |this, cx| {
                    if let Some(editor) = this.form.editor_mut(&cancel_id) {
                        editor.mode = EditorMode::Editing;
                        cx.notify();
                    }
                });
            },
        )
    });
    let button = if confirming { "exec" } else { "arm" };
    let id = id.to_owned();
    div()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(6.))
        .children(cancel)
        .child(pill_button(
            SharedString::from(format!("conn-rev-{button}-{id}")),
            "キーを削除",
            Tone::Danger.icon(Icon::Trash).enabled(live),
            theme,
            move |_, cx| {
                pane.update(cx, |this, cx| {
                    if let Some(editor) = this
                        .form
                        .editor_mut(&id)
                        .filter(|editor| editor.mode == mode)
                    {
                        editor.mode = if confirming {
                            EditorMode::Editing
                        } else {
                            EditorMode::ConfirmRevoke
                        };
                        if confirming {
                            this.hub
                                .update(cx, |hub, cx| hub.revoke_secret(id.clone(), cx));
                        }
                        cx.notify();
                    }
                });
            },
        ))
}

pub(super) fn render_enable_row(
    conn: &Connection,
    live: bool,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let conn_toggle = conn.clone();
    let hub_toggle = hub.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("接続を有効にする"),
        )
        .child(toggle_switch(
            SharedString::from(format!("conn-enable-{}", conn.id)),
            conn.enabled,
            live,
            theme,
            move |_, cx| {
                hub_toggle.update(cx, |hub, cx| {
                    hub.set_connection_enabled(&conn_toggle, !conn_toggle.enabled, cx);
                });
            },
        ))
}

pub(super) fn error_banner(check: Option<&ConnectionCheck>, theme: &Theme) -> Option<gpui::Div> {
    check
        .filter(|c| !c.healthy)
        .and_then(|c| c.failure.as_ref())
        .filter(|f| f.code != "unsupported" && !f.message.trim().is_empty())
        .map(|f| {
            div()
                .rounded(crate::ui::theme::radius::medium())
                .bg(theme.danger_soft)
                .px(px(12.))
                .py(px(8.))
                .text_xs()
                .text_color(theme.danger_fg)
                .child(f.message.clone())
        })
}

pub(super) fn render_editor_actions(
    conn: &Connection,
    payload: EditorSavePayload,
    hub: &Entity<Hub>,
    theme: &Theme,
) -> gpui::Div {
    let id = &conn.id;
    let rev = conn.revision;
    let delete_id = id.clone();
    let delete_title = format!("「{}」を削除しますか？", conn.name);
    let hub_delete = hub.clone();
    let hub_save = hub.clone();
    let conn_for_save = conn.clone();

    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap(px(8.))
        .child(pill_button(
            SharedString::from(format!("conn-delete-{id}")),
            "接続を削除",
            Tone::Danger.icon(Icon::Trash).enabled(payload.live),
            theme,
            move |window, cx| {
                let hub = hub_delete.clone();
                let id = delete_id.clone();
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    &delete_title,
                    Some("この接続の設定、保存したキー、コンパニオンへのモデル割り当てを削除します。"),
                    &["キャンセル", "削除"],
                    cx,
                );
                cx.spawn(async move |cx| {
                    if answer.await == Ok(1) {
                        cx.update(|cx| {
                            hub.update(cx, |hub, cx| hub.delete_connection(id, rev, cx));
                        });
                    }
                })
                .detach();
            },
        ))
        .child(pill_button(
            SharedString::from(format!("conn-save-{id}")),
            "保存",
            Tone::Primary.icon(Icon::Check).enabled(payload.can_save),
            theme,
            move |_, cx| {
                let Ok(args) = &payload.next_args else {
                    return;
                };
                let mut input = apply_connection_edits(
                    &conn_for_save,
                    &ConnectionEditSpec {
                        name: &payload.next_name,
                        endpoint_url: &payload.next_ep,
                        process: payload.next_cmd.as_deref().map(|cmd| (cmd, args.as_slice())),
                        list_raw: &payload.next_list,
                        dimensions_raw: payload.next_dim.as_deref(),
                        options_json: payload.next_options_json.as_deref(),
                    },
                );
                if let Some(Ok(extra)) = &payload.next_mcp_extra {
                    extra.clone().apply_to(&mut input);
                }
                hub_save.update(cx, |hub, cx| hub.upsert_connection("接続設定の保存", input, cx));
            },
        ))
}
