//! Remote host preference controls and asynchronous saves.

use super::{DevicePane, row_frame};
use crate::{
    host::remote::{RemoteEdit, RemoteProfile},
    input::Draft,
    ui::{
        icons::Icon,
        studio::labeled_field,
        theme::Theme,
        widgets::{ButtonStyle, Tone, pill_button, status_pill},
    },
};
use gpui::{AnyElement, Context, Entity, FontWeight, div, prelude::*, px};

#[derive(Debug, Default)]
pub(super) enum RemoteStatus {
    #[default]
    Idle,
    Saving,
    Saved,
    Failed(crate::Connect),
}

pub(super) fn render_remote_host_section(
    pane: &mut DevicePane,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> (AnyElement, AnyElement) {
    let saved = RemoteProfile::read();
    let has_current = !matches!(saved, Ok(None));
    let current_remote = saved.as_ref().ok().and_then(Option::as_ref);
    let remote_open = pane.form.remote_host_open;
    let pane_remote_toggle = cx.entity();
    let remote_heading = div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_size(px(18.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("ホスト接続先"),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("既定はローカルホスト（自動検出）です。Tailscale Serve や HTTPS リバースプロキシ経由で遠隔ホストへ接続することもできます。"),
        )
        .into_any_element();

    let form = remote_open.then(|| {
        let endpoint = current_remote.map(|profile| profile.endpoint.as_str());
        render_remote_host_form(pane, endpoint, has_current, theme, cx)
    });
    let status = match saved {
        Ok(Some(profile)) if !profile.needs_registration() => {
            format!("遠隔 HTTPS ({})", profile.endpoint)
        }
        Ok(None) => "ローカル（自動検出）".to_owned(),
        _ => "遠隔設定を再登録してください".to_owned(),
    };
    let remote_card = div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .flex()
        .flex_col()
        .child(row_frame(
            false,
            "次回起動時の接続先",
            Some(status_pill(status, None, theme.bg_blush, theme.ink_muted)),
            pill_button(
                "dev-remote-toggle",
                if remote_open {
                    "閉じる"
                } else {
                    "遠隔設定"
                },
                ButtonStyle::new(if remote_open { Tone::Soft } else { Tone::Ghost }),
                theme,
                move |_, cx| {
                    pane_remote_toggle.update(cx, |this, cx| {
                        this.form.remote_host_open = !this.form.remote_host_open;
                        cx.notify();
                    });
                },
            ),
            theme,
        ))
        .when_some(form, gpui::ParentElement::child)
        .into_any_element();

    (remote_heading, remote_card)
}

fn render_remote_host_form(
    pane: &mut DevicePane,
    current_remote: Option<&str>,
    has_current: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> AnyElement {
    let remote_ep_d = pane.drafts.draft(
        cx,
        "remote:endpoint",
        "https://ene.example.com/rpc",
        current_remote,
        theme,
    );
    let remote_tok_d =
        pane.drafts
            .masked_draft(cx, "remote:token", "Owner Bearer トークン", true, theme);
    let ep_val = remote_ep_d.read(cx).text().trim().to_owned();
    let has_token = !remote_tok_d.read(cx).text().trim().is_empty();
    let ep_validation = (!ep_val.is_empty())
        .then(|| {
            RemoteProfile::validate_endpoint(&ep_val)
                .err()
                .map(|e| e.to_string())
        })
        .flatten();
    let saving = matches!(pane.form.remote_status, RemoteStatus::Saving);
    let can_save_remote = !saving && !ep_val.is_empty() && has_token && ep_validation.is_none();
    let pane_save_remote = cx.entity();
    let pane_clear_remote = pane_save_remote.clone();
    let tok_clear = remote_tok_d.clone();
    let tok_save = remote_tok_d.clone();
    let ep_clear = remote_ep_d.clone();
    let ep_save = remote_ep_d.clone();
    let status = match &pane.form.remote_status {
        RemoteStatus::Idle => None,
        RemoteStatus::Saving => Some(("保存中…".into(), theme.ink_muted)),
        RemoteStatus::Saved => Some((
            "保存しました。次回起動時に適用されます。".into(),
            theme.ink_muted,
        )),
        RemoteStatus::Failed(error) => Some((error.to_string(), theme.danger_fg)),
    };

    div()
        .w_full()
        .border_t_1()
        .border_color(theme.border)
        .p(px(16.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(labeled_field(
            "遠隔エンドポイント（HTTPS /rpc 必須）",
            remote_ep_d,
            theme,
        ))
        .child(labeled_field("認証トークン（Bearer）", remote_tok_d, theme))
        .when_some(ep_validation, |this, err| {
            this.child(div().text_xs().text_color(theme.danger_fg).child(err))
        })
        .when_some(status, |this, (label, color)| {
            this.child(div().text_xs().text_color(color).child(label))
        })
        .child(
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(pill_button(
                    "dev-remote-clear",
                    "ローカルに戻す",
                    Tone::Ghost.enabled(has_current && !saving),
                    theme,
                    move |_, cx| {
                        pane_clear_remote.update(cx, |pane, cx| {
                            edit_remote(
                                pane,
                                RemoteEdit::Clear,
                                vec![ep_clear.clone(), tok_clear.clone()],
                                cx,
                            );
                        });
                    },
                ))
                .child(pill_button(
                    "dev-remote-save",
                    "遠隔接続先を保存",
                    Tone::Primary.icon(Icon::Check).enabled(can_save_remote),
                    theme,
                    move |_, cx| {
                        pane_save_remote.update(cx, |pane, cx| {
                            let edit = RemoteEdit::Save {
                                endpoint: ep_save.read(cx).text().trim().to_owned(),
                                token: tok_save.read(cx).text().trim().to_owned(),
                            };
                            edit_remote(pane, edit, vec![tok_save.clone()], cx);
                        });
                    },
                )),
        )
        .into_any_element()
}

fn edit_remote(
    pane: &mut DevicePane,
    edit: RemoteEdit,
    drafts: Vec<Entity<Draft>>,
    cx: &mut Context<DevicePane>,
) {
    if matches!(pane.form.remote_status, RemoteStatus::Saving) {
        return;
    }
    pane.form.remote_status = RemoteStatus::Saving;
    let task = cx
        .background_executor()
        .spawn(async move { edit.persist() });
    cx.spawn(async move |this, cx| {
        let outcome = task.await;
        let _observed = this.update(cx, |pane, cx| {
            pane.form.remote_status = match outcome {
                Ok(()) => {
                    for draft in drafts {
                        draft.update(cx, Draft::clear);
                    }
                    RemoteStatus::Saved
                }
                Err(error) => RemoteStatus::Failed(error),
            };
            cx.notify();
        });
    })
    .detach();
    cx.notify();
}
