//! `この端末` chapter in `SettingsStudio`: active client handoff, mic/speaker toggles,
//! 3D avatar scale & re-center, capability tags, and permission policy rules.

use super::SettingsStudio;
use super::icons::Icon;
use super::studio::labeled_field;
use super::theme::Theme;
use super::widgets::{Tone, disclosure_row, kind_pill, pill_button, status_pill, toggle_switch};
use crate::WORKSPACE_FILE_CAPABILITIES;
use crate::state::{Device, Link};
use ene_rpc as w;
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

const SKIP_FILE_CONFIRMATION_PREFIX: &str = "desktop.skip-file-confirmation:";
const SKIP_TOOL_CONFIRMATION_ID: &str = "desktop.skip-tool-confirmation";

fn is_file_confirmation_rule(rule: &w::PolicyRule) -> bool {
    rule.id
        .strip_prefix(SKIP_FILE_CONFIRMATION_PREFIX)
        .is_some_and(|capability| WORKSPACE_FILE_CAPABILITIES.contains(&capability))
}

fn tool_confirmation_request(skip: bool) -> w::UpdatePolicyRulesRequest {
    let mut request = w::UpdatePolicyRulesRequest {
        remove_ids: WORKSPACE_FILE_CAPABILITIES
            .iter()
            .map(|capability| format!("{SKIP_FILE_CONFIRMATION_PREFIX}{capability}"))
            .collect(),
        ..Default::default()
    };
    if skip {
        request.add_or_update.push(w::PolicyRule {
            id: SKIP_TOOL_CONFIRMATION_ID.into(),
            tool_name: "*".into(),
            pattern: "*".into(),
            effect: w::PolicyEffect::Allow.into(),
            ..Default::default()
        });
    } else {
        request.remove_ids.push(SKIP_TOOL_CONFIRMATION_ID.into());
    }
    request
}

impl SettingsStudio {
    pub(crate) fn render_device(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let (
            live,
            link,
            is_active_client,
            has_companion,
            devices,
            mic_muted,
            output_muted,
            eyes_enabled,
            eyes_status,
            avatar_scale,
            rules,
            policy_texts,
            client_caps,
            tray_resident,
            pod_visible,
        ) = {
            let hub = self.hub.read(cx);
            let our_id = hub.options.id.clone();
            let caps = hub
                .model
                .snapshot()
                .and_then(|s| s.clients.iter().find(|c| c.id == our_id))
                .map(|c| c.capabilities.clone())
                .unwrap_or_default();
            let rules = hub
                .model
                .snapshot()
                .map(|s| s.policy_rules.clone())
                .unwrap_or_default();
            let policy_texts = hub.model.policy_texts().to_vec();
            (
                hub.live(),
                hub.model.link.clone(),
                hub.model.is_active_client(&our_id),
                hub.model.companion_id().is_some(),
                hub.model.devices.clone(),
                hub.model.mic_muted,
                hub.model.output_muted,
                hub.model.eyes_enabled,
                hub.model.eyes_status.clone(),
                hub.model.avatar_scale,
                rules,
                policy_texts,
                caps,
                hub.is_tray_resident(),
                hub.is_pod_visible(),
            )
        };

        let header = div()
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
                    .child("この端末"),
            )
            .when(tray_resident, |this| {
                this.child(pill_button(
                    "dev-quit-app",
                    "ene を終了",
                    Some(Icon::Close),
                    Tone::Danger,
                    true,
                    theme,
                    |_, cx| cx.quit(),
                ))
            });

        let (link_text, link_dot, link_bg, link_fg) = match &link {
            Link::Live { version } => (
                format!("接続中 (v{version})"),
                theme.mint,
                theme.mint_soft,
                theme.mint_fg,
            ),
            Link::Connecting { .. } => (
                "接続中…".to_owned(),
                theme.amber,
                theme.amber_soft,
                theme.amber_fg,
            ),
            _ => (
                "未接続".to_owned(),
                theme.danger,
                theme.danger_soft,
                theme.danger_fg,
            ),
        };

        let hub_activate = self.hub.clone();
        let hub_pod = self.hub.clone();
        let hub_mic = self.hub.clone();
        let hub_out = self.hub.clone();
        let hub_eyes = self.hub.clone();
        let hub_center = self.hub.clone();
        #[cfg(windows)]
        let (computer_use, computer_granted) = {
            let hub = self.hub.read(cx);
            let granted = hub
                .model
                .snapshot()
                .and_then(|snapshot| {
                    snapshot
                        .clients
                        .iter()
                        .find(|client| client.id == hub.options.id)
                })
                .is_some_and(|client| {
                    client
                        .granted_capabilities
                        .iter()
                        .any(|name| name == w::COMPUTER_USE)
                });
            (hub.model.computer_use_enabled, granted)
        };
        #[cfg(windows)]
        let hub_computer = self.hub.clone();

        let mic_configured = self
            .hub
            .read(cx)
            .has_audio_connection(w::ConnectionKind::Stt);
        let output_configured = self
            .hub
            .read(cx)
            .has_audio_connection(w::ConnectionKind::Tts);
        let mic_ready = mic_configured && matches!(devices.input, Device::Ready(_));
        let output_ready = output_configured && matches!(devices.output, Device::Ready(_));
        let avatar_ready = matches!(devices.avatar, Device::Ready(_));
        let eyes_supported = crate::perception::supported();
        let eyes_label = match (
            eyes_enabled,
            live && is_active_client,
            eyes_status.as_deref(),
        ) {
            _ if !eyes_supported => "この環境では取得不可",
            (false, _, _) => "オフ",
            (_, false, _) => "担当端末になるまで待機",
            (_, _, Some("unavailable")) => "目のモデルに接続できません",
            (_, _, Some("image")) if cfg!(windows) => "画面と活動を評価",
            (_, _, Some("events" | "image")) => "活動イベントを評価",
            _ => "モデル確認中",
        };
        let mut scale_chips = Vec::new();
        for pct in [75.0f32, 100.0, 125.0, 150.0] {
            let selected = (avatar_scale - pct).abs() < 1.0;
            let hub_scale = self.hub.clone();
            scale_chips.push(pill_button(
                SharedString::from(format!("dev-scale-{}", pct as u32)),
                format!("{}%", pct as u32),
                None,
                if selected { Tone::Primary } else { Tone::Ghost },
                avatar_ready,
                theme,
                move |_, cx| {
                    hub_scale.update(cx, |hub, cx| hub.set_avatar_scale(pct, cx));
                },
            ));
        }

        let mut cap_pills = Vec::new();
        for cap in client_caps {
            cap_pills.push(kind_pill(cap, theme));
        }
        if cap_pills.is_empty() {
            cap_pills.push(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("登録なし")
                    .into_any_element(),
            );
        }

        let device_card = div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .flex()
            .flex_col()
            .child(row_frame(
                false,
                "対話の担当端末",
                Some(status_pill(link_text, Some(link_dot), link_bg, link_fg)),
                if is_active_client {
                    status_pill(
                        "この端末で対話中",
                        Some(theme.mint),
                        theme.mint_soft,
                        theme.mint_fg,
                    )
                } else {
                    pill_button(
                        "dev-activate-this",
                        "この端末に切り替える",
                        Some(Icon::Check),
                        Tone::Primary,
                        live && has_companion,
                        theme,
                        move |_, cx| {
                            hub_activate.update(cx, |hub, cx| hub.activate_this_client(cx))
                        },
                    )
                },
                theme,
            ))
            .child(row_frame(
                true,
                "声で話す",
                Some(if mic_configured {
                    device_pill(&devices.input, !mic_muted, theme)
                } else {
                    status_pill(
                        "音声認識の接続が必要",
                        None,
                        theme.amber_soft,
                        theme.amber_fg,
                    )
                }),
                toggle_switch(
                    "dev-mic-toggle",
                    mic_ready && !mic_muted,
                    mic_ready,
                    theme,
                    move |_, cx| {
                        hub_mic.update(cx, |hub, cx| hub.toggle_mic_mute(cx));
                    },
                ),
                theme,
            ))
            .child(row_frame(
                true,
                "返事を読み上げる",
                Some(if output_configured {
                    device_pill(&devices.output, !output_muted, theme)
                } else {
                    status_pill(
                        "読み上げの接続が必要",
                        None,
                        theme.amber_soft,
                        theme.amber_fg,
                    )
                }),
                toggle_switch(
                    "dev-out-toggle",
                    output_ready && !output_muted,
                    output_ready,
                    theme,
                    move |_, cx| {
                        hub_out.update(cx, |hub, cx| hub.toggle_output_mute(cx));
                    },
                ),
                theme,
            ))
            .child(row_frame(
                true,
                "目",
                Some(status_pill(eyes_label, None, theme.bg_blush, theme.ink_muted)),
                toggle_switch(
                    "dev-eyes-toggle",
                    eyes_enabled,
                    eyes_supported || eyes_enabled,
                    theme,
                    move |_, cx| hub_eyes.read(cx).set_eyes_enabled(!eyes_enabled),
                ),
                theme,
            ))
            .child(
                div()
                    .px(px(16.))
                    .pb(px(12.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(if cfg!(windows) {
                        "オンの間、活動情報をモデルの提供元に送ります。画像対応モデルには操作中のモニターの画像も送ります。"
                    } else {
                        "オンの間、ウィンドウと活動情報を、設定されたモデルの提供元に送ります。"
                    }),
            )
            .when(cfg!(windows), |this| {
                #[cfg(windows)]
                {
                    this
                    .child(row_frame(
                        true,
                        "手に画面操作を許可",
                        (live && computer_use && !computer_granted).then(|| status_pill(
                            "ホストの許可が必要",
                            None,
                            theme.amber_soft,
                            theme.amber_fg,
                        )),
                        toggle_switch("dev-computer-use", computer_use, true, theme, move |_, cx| {
                            hub_computer.read(cx).set_computer_use_enabled(!computer_use);
                        }),
                        theme,
                    ))
                    .child(
                        div()
                            .px(px(16.))
                            .pb(px(12.))
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child("オンの間、手はこの端末のアプリを操作できます。作業で取得した画面は作業モデルの提供元へ送ります。"),
                    )
                }
                #[cfg(not(windows))]
                this
            })
            .child(row_frame(
                true,
                "対話ウィンドウ",
                Some(if pod_visible {
                    status_pill("表示中", Some(theme.mint), theme.mint_soft, theme.mint_fg)
                } else {
                    status_pill(
                        "トレイ待機中",
                        Some(theme.ink_placeholder),
                        theme.bg_blush,
                        theme.ink_muted,
                    )
                }),
                pill_button(
                    "dev-pod-toggle",
                    if pod_visible { "隠す" } else { "開く" },
                    Some(if pod_visible {
                        Icon::Minimize
                    } else {
                        Icon::ChatBubble
                    }),
                    if pod_visible { Tone::Ghost } else { Tone::Soft },
                    true,
                    theme,
                    move |_, cx| {
                        hub_pod.update(cx, |hub, cx| {
                            if pod_visible {
                                hub.hide_pod(cx);
                            } else {
                                hub.open_pod(None, cx);
                            }
                        });
                    },
                ),
                theme,
            ))
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px(px(16.))
                    .py(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(12.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(10.))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(theme.ink)
                                            .child("アバター表示"),
                                    )
                                    .child(device_pill(&devices.avatar, avatar_ready, theme)),
                            )
                            .child(pill_button(
                                "dev-avatar-center",
                                "中央に戻す",
                                Some(Icon::Crosshair),
                                Tone::Ghost,
                                avatar_ready,
                                theme,
                                move |_, cx| hub_center.read(cx).center_avatar(),
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(10.))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.ink_muted)
                                    .child("表示倍率"),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .flex_wrap()
                                    .gap(px(6.))
                                    .children(scale_chips),
                            ),
                    ),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px(px(16.))
                    .py(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.ink)
                            .child("この端末の公開機能"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(6.))
                            .children(cap_pills),
                    ),
            );

        // Policy rules section
        let policy_heading = div()
            .text_size(px(18.))
            .font_weight(FontWeight::BOLD)
            .text_color(theme.ink)
            .child("許可ルール");

        let skip_tool_confirmation = rules
            .iter()
            .any(|rule| rule.id == SKIP_TOOL_CONFIRMATION_ID);
        let skip_files_only =
            !skip_tool_confirmation && rules.iter().any(is_file_confirmation_rule);
        let hub_confirmation = self.hub.clone();
        let hub_revoke_legacy = self.hub.clone();
        let mut rule_items: Vec<AnyElement> = Vec::new();
        for rule in rules
            .into_iter()
            .filter(|rule| rule.id != SKIP_TOOL_CONFIRMATION_ID && !is_file_confirmation_rule(rule))
        {
            let (eff_text, dot, bg, fg) = match rule.effect.as_known() {
                Some(w::PolicyEffect::Allow) => {
                    ("許可", theme.mint, theme.mint_soft, theme.mint_fg)
                }
                Some(w::PolicyEffect::Deny) => {
                    ("拒否", theme.danger, theme.danger_soft, theme.danger_fg)
                }
                _ => ("確認", theme.amber, theme.amber_soft, theme.amber_fg),
            };
            let armed = self.delete_armed_policy.as_deref() == Some(rule.id.as_str());
            let studio_arm = cx.entity();
            let studio_cancel = cx.entity();
            let hub_remove = self.hub.clone();
            let arm_id = rule.id.clone();
            let del_id = rule.id.clone();

            let trailing = if armed {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(pill_button(
                        SharedString::from(format!("pol-cancel-{}", rule.id)),
                        "キャンセル",
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_cancel.update(cx, |this, cx| {
                                this.delete_armed_policy = None;
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("pol-exec-{}", rule.id)),
                        "確定",
                        Some(Icon::Trash),
                        Tone::Danger,
                        live,
                        theme,
                        move |_, cx| {
                            hub_remove
                                .update(cx, |hub, cx| hub.remove_policy_rule(del_id.clone(), cx));
                        },
                    ))
                    .into_any_element()
            } else {
                pill_button(
                    SharedString::from(format!("pol-arm-{}", rule.id)),
                    "削除",
                    Some(Icon::Trash),
                    Tone::Danger,
                    live,
                    theme,
                    move |_, cx| {
                        studio_arm.update(cx, |this, cx| {
                            this.delete_armed_policy = Some(arm_id.clone());
                            cx.notify();
                        });
                    },
                )
            };

            rule_items.push(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px(px(16.))
                    .py(px(12.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.))
                            .child(status_pill(eff_text, Some(dot), bg, fg))
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.ink)
                                    .child(rule.tool_name),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(rule.pattern),
                            ),
                    )
                    .child(trailing)
                    .into_any_element(),
            );
        }

        if rule_items.is_empty() {
            rule_items.push(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px(px(16.))
                    .py(px(14.))
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("保存されているルールはありません（「常に許可」を選ぶと自動的に追加されます）。")
                    .into_any_element(),
            );
        }

        let policy_card = div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .flex()
            .flex_col()
            .child(row_frame(
                false,
                "ツール操作の確認",
                skip_files_only.then(|| {
                    status_pill(
                        "ファイル操作のみ省略中",
                        None,
                        theme.amber_soft,
                        theme.amber_fg,
                    )
                }),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .when(skip_files_only, |buttons| {
                        buttons.child(pill_button(
                            "dev-file-confirmation-restore",
                            "省略を解除",
                            None,
                            Tone::Ghost,
                            live,
                            theme,
                            move |_, cx| {
                                hub_revoke_legacy.update(cx, |hub, cx| {
                                    hub.update_policy_rules(
                                        "ツール操作の確認設定",
                                        tool_confirmation_request(false),
                                        cx,
                                    );
                                });
                            },
                        ))
                    })
                    .child(pill_button(
                        "dev-tool-confirmation",
                        if skip_tool_confirmation {
                            "省略を解除"
                        } else {
                            "すべて省略"
                        },
                        None,
                        if skip_tool_confirmation {
                            Tone::Soft
                        } else {
                            Tone::Ghost
                        },
                        live,
                        theme,
                        move |_, cx| {
                            hub_confirmation.update(cx, |hub, cx| {
                                hub.update_policy_rules(
                                    "ツール操作の確認設定",
                                    tool_confirmation_request(!skip_tool_confirmation),
                                    cx,
                                );
                            });
                        },
                    ))
                    .into_any_element(),
                theme,
            ))
            .children(rule_items);

        let studio_add_pol = cx.entity();
        let add_open = self.adding_policy_open;
        let add_toggle = disclosure_row(
            "dev-policy-add-disclosure",
            "ルールを手動追加",
            add_open,
            theme,
            move |_, cx| {
                studio_add_pol.update(cx, |this, cx| {
                    this.adding_policy_open = !this.adding_policy_open;
                    cx.notify();
                });
            },
        );
        let add_panel = if add_open {
            let tool_d = self.draft(cx, "policy:tool", "例: device.files / bash", None, theme);
            let pattern_d = self.draft(cx, "policy:pattern", "例: * / C:/work/*", None, theme);
            let tool_val = tool_d.read(cx).text().trim().to_owned();
            let pattern_val = pattern_d.read(cx).text().trim().to_owned();
            let eff_choice = self.new_policy_effect;

            let eff_chip = |eff: w::PolicyEffect, label: &'static str| {
                let selected = eff_choice == eff;
                let studio = cx.entity();
                pill_button(
                    SharedString::from(format!("dev-pol-eff-{eff:?}")),
                    label,
                    None,
                    if selected { Tone::Primary } else { Tone::Ghost },
                    true,
                    theme,
                    move |_, cx| {
                        studio.update(cx, |this, cx| {
                            this.new_policy_effect = eff;
                            cx.notify();
                        });
                    },
                )
            };

            let hub_add = self.hub.clone();
            Some(
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
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(10.))
                            .child(div().flex_1().min_w_0().child(labeled_field(
                                "ツール名",
                                tool_d,
                                theme,
                            )))
                            .child(div().flex_1().min_w_0().child(labeled_field(
                                "対象パターン",
                                pattern_d,
                                theme,
                            ))),
                    )
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
                                    .gap(px(6.))
                                    .child(eff_chip(w::PolicyEffect::Allow, "許可"))
                                    .child(eff_chip(w::PolicyEffect::Confirm, "確認"))
                                    .child(eff_chip(w::PolicyEffect::Deny, "拒否")),
                            )
                            .child(pill_button(
                                "dev-pol-submit",
                                "追加",
                                Some(Icon::Plus),
                                Tone::Primary,
                                live && !tool_val.is_empty() && !pattern_val.is_empty(),
                                theme,
                                move |_, cx| {
                                    hub_add.update(cx, |hub, cx| {
                                        hub.add_policy_rule(
                                            tool_val.clone(),
                                            pattern_val.clone(),
                                            eff_choice,
                                            cx,
                                        );
                                    });
                                },
                            )),
                    ),
            )
        } else {
            None
        };

        let reset_history = cx.try_global::<super::HistoryReset>().cloned().map(|reset| {
            div()
                .w_full()
                .bg(theme.bg_surface)
                .border_1()
                .border_color(theme.border)
                .rounded(px(14.))
                .child(row_frame(
                    false,
                    "会話・作業の履歴と記憶",
                    None,
                    pill_button(
                        "dev-reset-history",
                        "履歴をリセット",
                        Some(Icon::Trash),
                        Tone::Danger,
                        live,
                        theme,
                        move |window, cx| {
                            let requested = reset.requested.clone();
                            let answer = window.prompt(
                                gpui::PromptLevel::Warning,
                                "履歴と記憶を消去しますか？",
                                Some("全コンパニオンの会話・作業履歴と記憶を消去して ene を再起動します。接続設定、API キー、キャラクター設定は保持されます。"),
                                &["キャンセル", "消去して再起動"],
                                cx,
                            );
                            cx.spawn(async move |cx| {
                                if answer.await == Ok(1) {
                                    requested.store(true, std::sync::atomic::Ordering::Relaxed);
                                    let _ = cx.update(|cx| cx.quit());
                                }
                            }).detach();
                        },
                    ),
                    theme,
                ))
                .when_some(reset.failure, |card, failure| {
                    card.child(
                        div()
                            .px(px(16.))
                            .pb(px(12.))
                            .text_xs()
                            .text_color(theme.danger_fg)
                            .child(failure),
                    )
                })
                .into_any_element()
        });

        let policy_text_heading = div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(18.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("自由文ポリシー（AI 補助判定の基準）"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("構造化ルールで未決の操作を Decision モデルが評価する際の参考基準（各 500 文字以内）です。強制的な禁止や毎回確認には上の構造化ルールを使ってください。"),
            );

        let mut text_items: Vec<AnyElement> = Vec::new();
        for (index, item) in policy_texts.iter().enumerate() {
            let editing = self.editing_policy_text_id.as_deref() == Some(item.id.as_str());
            let armed = self.delete_armed_policy_text.as_deref() == Some(item.id.as_str());
            if editing {
                let edit_d = self.description_draft(
                    cx,
                    format!("policy-text:edit:{}", item.id),
                    "例: プロジェクト外への書き込みは必ず確認する",
                    Some(&item.text),
                    theme,
                );
                let edit_val = edit_d.read(cx).text().trim().to_owned();
                let char_count = edit_val.chars().count();
                let valid = (1..=500).contains(&char_count);
                let studio_cancel = cx.entity();
                let studio_save = cx.entity();
                let hub_save = self.hub.clone();
                let save_id = item.id.clone();
                text_items.push(
                    div()
                        .when(index > 0, |this| {
                            this.border_t_1().border_color(theme.border)
                        })
                        .px(px(16.))
                        .py(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(edit_d)
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if char_count > 500 {
                                            theme.danger_fg
                                        } else {
                                            theme.ink_muted
                                        })
                                        .child(format!("{char_count} / 500 文字")),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .gap(px(6.))
                                        .child(pill_button(
                                            SharedString::from(format!(
                                                "poltxt-edit-cancel-{}",
                                                item.id
                                            )),
                                            "キャンセル",
                                            None,
                                            Tone::Ghost,
                                            true,
                                            theme,
                                            move |_, cx| {
                                                studio_cancel.update(cx, |this, cx| {
                                                    this.editing_policy_text_id = None;
                                                    cx.notify();
                                                });
                                            },
                                        ))
                                        .child(pill_button(
                                            SharedString::from(format!(
                                                "poltxt-edit-save-{}",
                                                item.id
                                            )),
                                            "保存",
                                            Some(Icon::Check),
                                            Tone::Primary,
                                            live && valid,
                                            theme,
                                            move |_, cx| {
                                                let val = edit_val.clone();
                                                let id = save_id.clone();
                                                hub_save.update(cx, |hub, cx| {
                                                    hub.upsert_policy_text(Some(id), val, cx);
                                                });
                                                studio_save.update(cx, |this, cx| {
                                                    this.editing_policy_text_id = None;
                                                    cx.notify();
                                                });
                                            },
                                        )),
                                ),
                        )
                        .into_any_element(),
                );
                continue;
            }
            let studio_edit = cx.entity();
            let studio_arm = cx.entity();
            let studio_cancel = cx.entity();
            let hub_remove = self.hub.clone();
            let edit_id = item.id.clone();
            let arm_id = item.id.clone();
            let del_id = item.id.clone();
            let trailing = if armed {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(pill_button(
                        SharedString::from(format!("poltxt-cancel-{}", item.id)),
                        "キャンセル",
                        None,
                        Tone::Ghost,
                        true,
                        theme,
                        move |_, cx| {
                            studio_cancel.update(cx, |this, cx| {
                                this.delete_armed_policy_text = None;
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("poltxt-exec-{}", item.id)),
                        "確定",
                        Some(Icon::Trash),
                        Tone::Danger,
                        live,
                        theme,
                        move |_, cx| {
                            hub_remove
                                .update(cx, |hub, cx| hub.remove_policy_text(del_id.clone(), cx));
                        },
                    ))
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(pill_button(
                        SharedString::from(format!("poltxt-edit-{}", item.id)),
                        "編集",
                        None,
                        Tone::Ghost,
                        live,
                        theme,
                        move |_, cx| {
                            studio_edit.update(cx, |this, cx| {
                                this.editing_policy_text_id = Some(edit_id.clone());
                                this.delete_armed_policy_text = None;
                                cx.notify();
                            });
                        },
                    ))
                    .child(pill_button(
                        SharedString::from(format!("poltxt-arm-{}", item.id)),
                        "削除",
                        Some(Icon::Trash),
                        Tone::Danger,
                        live,
                        theme,
                        move |_, cx| {
                            studio_arm.update(cx, |this, cx| {
                                this.delete_armed_policy_text = Some(arm_id.clone());
                                cx.notify();
                            });
                        },
                    ))
                    .into_any_element()
            };
            text_items.push(
                div()
                    .when(index > 0, |this| {
                        this.border_t_1().border_color(theme.border)
                    })
                    .px(px(16.))
                    .py(px(11.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .text_color(theme.ink)
                            .child(item.text.clone()),
                    )
                    .child(trailing)
                    .into_any_element(),
            );
        }

        let policy_text_card = div()
            .w_full()
            .bg(theme.bg_surface)
            .border_1()
            .border_color(theme.border)
            .rounded(px(14.))
            .when(text_items.is_empty(), |card| {
                card.child(
                    div()
                        .px(px(16.))
                        .py(px(12.))
                        .text_xs()
                        .text_color(theme.ink_muted)
                        .child("自由文ポリシーはまだありません。"),
                )
            })
            .children(text_items);

        let studio_add_poltxt = cx.entity();
        let add_txt_open = self.adding_policy_text_open;
        let add_txt_toggle = disclosure_row(
            "dev-policy-text-add-disclosure",
            "自由文ポリシーを追加",
            add_txt_open,
            theme,
            move |_, cx| {
                studio_add_poltxt.update(cx, |this, cx| {
                    this.adding_policy_text_open = !this.adding_policy_text_open;
                    cx.notify();
                });
            },
        );
        let add_txt_panel = if add_txt_open {
            let new_d = self.description_draft(
                cx,
                "policy-text:new",
                "例: 外部 URL へのデータ送信前には必ず確認を求める",
                None,
                theme,
            );
            let new_val = new_d.read(cx).text().trim().to_owned();
            let char_count = new_val.chars().count();
            let valid = (1..=500).contains(&char_count);
            let hub_add = self.hub.clone();
            let studio_clear = cx.entity();
            let draft_clear = new_d.clone();
            Some(
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
                    .child(labeled_field("ポリシー文（500 文字以内）", new_d, theme))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if char_count > 500 {
                                        theme.danger_fg
                                    } else {
                                        theme.ink_muted
                                    })
                                    .child(format!("{char_count} / 500 文字")),
                            )
                            .child(pill_button(
                                "dev-poltxt-submit",
                                "追加",
                                Some(Icon::Plus),
                                Tone::Primary,
                                live && valid,
                                theme,
                                move |_, cx| {
                                    let val = new_val.clone();
                                    hub_add.update(cx, |hub, cx| {
                                        hub.upsert_policy_text(None, val, cx);
                                    });
                                    draft_clear.update(cx, |draft, cx| draft.clear(cx));
                                    studio_clear.update(cx, |this, cx| {
                                        this.adding_policy_text_open = false;
                                        cx.notify();
                                    });
                                },
                            )),
                    ),
            )
        } else {
            None
        };

        let plugins = self.hub.read(cx).model.plugins.clone();
        let plugins_heading = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.ink)
                    .child("プラグイン管理"),
            )
            .child(pill_button(
                "dev-plugins-refresh",
                "更新",
                None,
                Tone::Ghost,
                live,
                theme,
                {
                    let hub = self.hub.clone();
                    move |_, cx| hub.update(cx, |h, cx| h.load_plugins(cx))
                },
            ));
        let plugins_card = div()
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
                        .child("インストール済みプラグインはありません。"),
                )
            })
            .children(plugins.into_iter().map(|plugin| {
                let id_toggle = plugin.id.clone();
                let id_remove = plugin.id.clone();
                let next_enabled = !plugin.enabled;
                let hub_toggle = self.hub.clone();
                let hub_remove = self.hub.clone();
                let subtitle = if plugin.extensions.is_empty() {
                    format!("v{}", plugin.version)
                } else {
                    format!("v{} · {}", plugin.version, plugin.extensions.join(", "))
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme.ink)
                                    .child(format!("{} ({})", plugin.name, plugin.id)),
                            )
                            .child(div().text_xs().text_color(theme.ink_muted).child(subtitle)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(6.))
                            .child(pill_button(
                                SharedString::from(format!("plugin-toggle-{}", plugin.id)),
                                if plugin.enabled {
                                    "無効化"
                                } else {
                                    "有効化"
                                },
                                None,
                                if plugin.enabled {
                                    Tone::Mint
                                } else {
                                    Tone::Ghost
                                },
                                live,
                                theme,
                                move |_, cx| {
                                    hub_toggle.update(cx, |h, cx| {
                                        h.set_plugin_enabled(id_toggle.clone(), next_enabled, cx);
                                    });
                                },
                            ))
                            .child(pill_button(
                                SharedString::from(format!("plugin-remove-{}", plugin.id)),
                                "削除",
                                None,
                                Tone::Danger,
                                live,
                                theme,
                                move |_, cx| {
                                    hub_remove.update(cx, |h, cx| {
                                        h.remove_plugin(id_remove.clone(), cx);
                                    });
                                },
                            )),
                    )
            }));

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .child(device_card)
            .child(policy_heading)
            .child(policy_card)
            .child(add_toggle)
            .when_some(add_panel, |this, panel| this.child(panel))
            .child(policy_text_heading)
            .child(policy_text_card)
            .child(add_txt_toggle)
            .when_some(add_txt_panel, |this, panel| this.child(panel))
            .child(plugins_heading)
            .child(plugins_card)
            .when_some(reset_history, |this, card| this.child(card))
            .into_any_element()
    }
}

fn row_frame(
    top_border: bool,
    title: &'static str,
    status: Option<AnyElement>,
    control: AnyElement,
    theme: &Theme,
) -> AnyElement {
    div()
        .when(top_border, |this| {
            this.border_t_1().border_color(theme.border)
        })
        .px(px(16.))
        .py(px(14.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child(title),
                )
                .when_some(status, |this, s| this.child(s)),
        )
        .child(control)
        .into_any_element()
}

fn device_pill(device: &Device, active_flag: bool, theme: &Theme) -> AnyElement {
    match device {
        Device::Ready(_) if active_flag => {
            status_pill("有効", Some(theme.mint), theme.mint_soft, theme.mint_fg)
        }
        Device::Ready(_) => status_pill(
            "消音",
            Some(theme.ink_muted),
            theme.bg_blush,
            theme.ink_muted,
        ),
        Device::Off => status_pill(
            "停止中",
            Some(theme.ink_placeholder),
            theme.bg_blush,
            theme.ink_muted,
        ),
        Device::Unavailable(_) => status_pill(
            "利用不可",
            Some(theme.danger),
            theme.danger_soft,
            theme.danger_fg,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_confirmation_updates_only_managed_rules() {
        let enabled = tool_confirmation_request(true);
        let disabled = tool_confirmation_request(false);
        assert!(disabled.add_or_update.is_empty());
        let [rule] = enabled.add_or_update.as_slice() else {
            panic!("expected one global rule")
        };
        assert_eq!(rule.id, SKIP_TOOL_CONFIRMATION_ID);
        assert_eq!(rule.tool_name, "*");
        assert_eq!(rule.pattern, "*");
        assert_eq!(rule.effect.as_known(), Some(w::PolicyEffect::Allow));
        assert_eq!(enabled.remove_ids.len(), WORKSPACE_FILE_CAPABILITIES.len());
        assert!(
            enabled
                .remove_ids
                .iter()
                .all(|id| is_file_confirmation_rule(&w::PolicyRule {
                    id: id.clone(),
                    ..Default::default()
                }))
        );
        assert_eq!(
            &disabled.remove_ids[..enabled.remove_ids.len()],
            &enabled.remove_ids
        );
        assert_eq!(
            disabled.remove_ids.last().unwrap(),
            SKIP_TOOL_CONFIRMATION_ID
        );
        assert!(!is_file_confirmation_rule(&w::PolicyRule {
            id: "manual-write-rule".into(),
            tool_name: "work.write".into(),
            ..Default::default()
        }));
    }
}
