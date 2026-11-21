//! Wasm plugin management and remote HTTPS host target UI sections for `この端末`.

use super::{DeviceSnapshot, row_frame};
use crate::ui::{
    Hub, SettingsStudio,
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{ButtonStyle, Tone, pill_button, status_pill},
};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

impl SettingsStudio {
    pub(super) fn render_plugins_section(
        &mut self,
        snap: &DeviceSnapshot,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, Option<AnyElement>, AnyElement) {
        let adding_plugin = self.device_form.adding_plugin_open;
        let studio_add_plugin = cx.entity();
        let plugins_heading = div()
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(10.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child("プラグイン管理"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("ローカルの Wasm プラグインパッケージ（ene-plugin.json）を導入・有効化・削除します。"),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(pill_button("dev-plugins-refresh", "更新", Tone::Ghost.icon(Icon::Refresh).enabled(snap.live), theme, {
                            let hub = self.hub.clone();
                            move |_, cx| hub.update(cx, |h, cx| h.load_plugins(cx))
                        }))
                    .child(pill_button("dev-plugins-toggle-add", if adding_plugin { "閉じる" } else { "追加" }, ButtonStyle::new(if adding_plugin { Tone::Ghost } else { Tone::Primary }).maybe_icon((!adding_plugin).then_some(Icon::Plus)).enabled(snap.live), theme, move |_, cx| {
                            studio_add_plugin.update(cx, |this, cx| {
                                this.device_form.adding_plugin_open =
                                    !this.device_form.adding_plugin_open;
                                cx.notify();
                            });
                        })),
            )
            .into_any_element();

        let add_plugin_panel =
            adding_plugin.then(|| self.render_add_plugin_panel(snap.live, theme, cx));
        let plugins_card = render_plugins_card(&self.hub, &snap.plugins, snap.live, theme);

        (plugins_heading, add_plugin_panel, plugins_card)
    }

    fn render_add_plugin_panel(
        &mut self,
        live: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plugin_path_d = self.draft(
            cx,
            "plugin:install-path",
            "プラグインフォルダのパス（ene-plugin.json を含むディレクトリ）",
            None,
            theme,
        );
        let path_val = plugin_path_d.read(cx).text().trim().to_owned();
        let draft_browse = plugin_path_d.clone();
        let draft_clear = plugin_path_d.clone();
        let hub_install = self.hub.clone();
        let studio_close = cx.entity();

        div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .p(px(16.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(labeled_field("パッケージフォルダ", plugin_path_d, theme))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(pill_button(
                        "dev-plugin-browse",
                        "フォルダを選択",
                        Tone::Soft,
                        theme,
                        move |_, cx| {
                            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                files: false,
                                directories: true,
                                multiple: false,
                                prompt: Some("プラグインパッケージのフォルダを選択".into()),
                            });
                            let draft = draft_browse.clone();
                            cx.spawn(async move |cx| {
                                if let Ok(Ok(Some(paths))) = rx.await
                                    && let Some(path) = paths.into_iter().next()
                                {
                                    let path_str = path.to_string_lossy().into_owned();
                                    let _ = cx.update(|cx| {
                                        draft.update(cx, |d, cx| d.set(path_str, cx));
                                    });
                                }
                            })
                            .detach();
                        },
                    ))
                    .child(pill_button(
                        "dev-plugin-install-submit",
                        "導入する",
                        Tone::Primary
                            .icon(Icon::Plus)
                            .enabled(live && !path_val.is_empty()),
                        theme,
                        move |_, cx| {
                            let path = path_val.clone();
                            hub_install.update(cx, |h, cx| h.install_plugin(path, cx));
                            draft_clear.update(cx, |d, cx| d.clear(cx));
                            studio_close.update(cx, |this, cx| {
                                this.device_form.adding_plugin_open = false;
                                cx.notify();
                            });
                        },
                    )),
            )
            .into_any_element()
    }

    pub(super) fn render_remote_host_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> (AnyElement, AnyElement) {
        let current_remote = Hub::current_remote_host();
        let remote_open = self.device_form.remote_host_open;
        let studio_remote_toggle = cx.entity();
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

        let form =
            remote_open.then(|| self.render_remote_host_form(current_remote.as_deref(), theme, cx));
        let remote_card = div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .flex()
            .flex_col()
            .child(row_frame(
                false,
                "接続モード",
                Some(match &current_remote {
                    Some(ep) => status_pill(
                        format!("遠隔 HTTPS ({ep})"),
                        Some(theme.mint),
                        theme.mint_soft,
                        theme.mint_fg,
                    ),
                    None => status_pill(
                        "ローカル（自動検出）",
                        Some(theme.mint),
                        theme.mint_soft,
                        theme.mint_fg,
                    ),
                }),
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
                        studio_remote_toggle.update(cx, |this, cx| {
                            this.device_form.remote_host_open = !this.device_form.remote_host_open;
                            cx.notify();
                        });
                    },
                ),
                theme,
            ))
            .when_some(form, |card, panel| card.child(panel))
            .into_any_element();

        (remote_heading, remote_card)
    }

    fn render_remote_host_form(
        &mut self,
        current_remote: Option<&str>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let remote_ep_d = self.draft(
            cx,
            "remote:endpoint",
            "https://ene.example.com/rpc",
            current_remote,
            theme,
        );
        let remote_tok_d =
            self.masked_draft(cx, "remote:token", "Owner Bearer トークン", true, theme);
        let ep_val = remote_ep_d.read(cx).text().trim().to_owned();
        let tok_val = remote_tok_d.read(cx).text().trim().to_owned();
        let ep_validation = (!ep_val.is_empty())
            .then(|| {
                ene_rpc::validate_remote_endpoint(&ep_val)
                    .err()
                    .map(|e| e.to_string())
            })
            .flatten();
        let can_save_remote = !ep_val.is_empty() && !tok_val.is_empty() && ep_validation.is_none();
        let hub_save_remote = self.hub.clone();
        let hub_clear_remote = self.hub.clone();
        let tok_clear = remote_tok_d.clone();
        let ep_clear = remote_ep_d.clone();
        let has_current = current_remote.is_some();

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
                        Tone::Ghost.enabled(has_current),
                        theme,
                        move |_, cx| {
                            hub_clear_remote.update(cx, |h, cx| h.clear_remote_host(cx));
                            ep_clear.update(cx, |d, cx| d.clear(cx));
                            tok_clear.update(cx, |d, cx| d.clear(cx));
                        },
                    ))
                    .child(pill_button(
                        "dev-remote-save",
                        "遠隔接続先を保存",
                        Tone::Primary.icon(Icon::Check).enabled(can_save_remote),
                        theme,
                        move |_, cx| {
                            let ep = ep_val.clone();
                            let tok = tok_val.clone();
                            hub_save_remote.update(cx, |h, cx| {
                                let _ = h.save_remote_host(ep, tok, cx);
                            });
                        },
                    )),
            )
            .into_any_element()
    }
}

fn render_plugins_card(
    hub: &gpui::Entity<Hub>,
    plugins: &[ene_rpc::PluginInfo],
    live: bool,
    theme: &Theme,
) -> AnyElement {
    div()
        .w_full()
        .rounded(px(14.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.bg_surface)
        .px(px(16.))
        .py(px(12.))
        .flex()
        .flex_col()
        .gap(px(8.))
        .when(plugins.is_empty(), |card| {
            card.child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("インストール済みプラグインはありません。右上の「追加」からローカルパッケージを導入できます。"),
            )
        })
        .children(plugins.iter().map(|plugin| {
            let id_toggle = plugin.id.clone();
            let id_remove = plugin.id.clone();
            let next_enabled = !plugin.enabled;
            let hub_toggle = hub.clone();
            let hub_remove = hub.clone();
            let subtitle = if plugin.extensions.is_empty() {
                format!("v{}", plugin.version)
            } else {
                format!("v{} · {}", plugin.version, plugin.extensions.join(", "))
            };
            div()
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.ink)
                                .truncate()
                                .child(format!("{} ({})", plugin.name, plugin.id)),
                        )
                        .child(div().text_xs().text_color(theme.ink_muted).child(subtitle)),
                )
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .gap(px(6.))
                        .child(pill_button(SharedString::from(format!("plugin-toggle-{}", plugin.id)), if plugin.enabled { "無効化" } else { "有効化" }, ButtonStyle::new(if plugin.enabled { Tone::Mint } else { Tone::Ghost }).enabled(live), theme, move |_, cx| {
                                hub_toggle.update(cx, |h, cx| {
                                    h.set_plugin_enabled(id_toggle.clone(), next_enabled, cx);
                                });
                            }))
                        .child(pill_button(SharedString::from(format!("plugin-remove-{}", plugin.id)), "削除", Tone::Danger.enabled(live), theme, move |_, cx| {
                                hub_remove.update(cx, |h, cx| h.remove_plugin(id_remove.clone(), cx));
                            })),
                )
        }))
        .into_any_element()
}
