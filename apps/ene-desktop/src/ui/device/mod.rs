//! `この端末` chapter in `SettingsStudio`: active client handoff, mic/speaker toggles,
//! 3D avatar scale & re-center, capability tags, plugins, remote host, and policy rules.

use crate::ui::widgets::surface_card;
use gpui::component::StyledExt as _;
mod hardware;
mod plugins;
mod policy;
mod remote;

use super::icons::Icon;
use super::theme::Theme;
use super::widgets::{Tone, pill_button, status_pill};
use super::{DraftStore, Hub};
use crate::WORKSPACE_FILE_CAPABILITIES;
use crate::state::Device;
use ene_client_core::domain::{
    PolicyEffect, PolicyRule, PolicyRulesUpdate, SKIP_TOOL_CONFIRMATION_ID,
    is_file_confirmation_rule as domain_is_file_confirmation, tool_confirmation_update,
};
use gpui::{AnyElement, Context, Entity, FontWeight, Window, div, prelude::*, px};

/// Form and arming state owned by [`DevicePane`].
pub(super) struct DeviceFormState {
    pub(super) delete_armed_policy: Option<String>,
    pub(super) adding_policy_open: bool,
    pub(super) new_policy_effect: PolicyEffect,
    pub(super) delete_armed_policy_text: Option<String>,
    pub(super) editing_policy_text_id: Option<String>,
    pub(super) adding_policy_text_open: bool,
    pub(super) adding_plugin_open: bool,
    pub(super) remote_host_open: bool,
    remote_status: remote::RemoteStatus,
    remote_profile: Option<Result<Option<crate::RemoteProfile>, crate::Connect>>,
    pub(super) skip_confirm_armed: bool,
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
            remote_status: remote::RemoteStatus::default(),
            remote_profile: None,
            skip_confirm_armed: false,
        }
    }
}

/// Device, plugin, remote-host, and policy chapter rendered as an independent GPUI pane entity.
pub(crate) struct DevicePane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    pub(super) form: DeviceFormState,
}

impl std::fmt::Debug for DevicePane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevicePane").finish_non_exhaustive()
    }
}

pub(super) fn is_file_confirmation_rule(rule: &PolicyRule) -> bool {
    domain_is_file_confirmation(rule, WORKSPACE_FILE_CAPABILITIES)
}

pub(super) fn tool_confirmation_request(skip: bool) -> PolicyRulesUpdate {
    tool_confirmation_update(skip, WORKSPACE_FILE_CAPABILITIES)
}

impl DevicePane {
    pub(crate) fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        remote::load(cx);
        Self {
            hub,
            drafts: DraftStore::default(),
            form: DeviceFormState::default(),
        }
    }

    pub(crate) fn on_leave(&mut self) {
        self.form.skip_confirm_armed = false;
    }
}

impl gpui::Render for DevicePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, live, header, device_card, rules, policy_texts, plugins) = {
            let hub = self.hub.read(cx);
            let theme = hub.theme();
            // The session can replace the RPC connection while the view is rendering.
            let live = hub.live();
            let header = render_device_header(hub.is_tray_resident(), &theme);
            let device_card = hardware::render_device_status_card(self, hub, live, &theme);
            let rules = hub
                .model
                .snapshot()
                .map_or_default(|s| s.policy_rules.clone());
            (
                theme,
                live,
                header,
                device_card,
                rules,
                hub.model.policy_texts().to_vec(),
                hub.model.connections.plugins.clone(),
            )
        };
        let (policy_heading, policy_card, add_toggle, add_panel) =
            policy::render_policy_rules_section(self, &rules, live, &theme, cx);
        let (policy_text_heading, policy_text_card, add_txt_toggle, add_txt_panel) =
            policy::render_policy_texts_section(self, &policy_texts, live, &theme, cx);
        let (plugins_heading, add_plugin_panel, plugins_card) =
            plugins::render_plugins_section(self, &plugins, live, &theme, cx);
        let (remote_heading, remote_card) = remote::render_remote_host_section(self, &theme, cx);
        let reset_history = render_history_reset_card(live, &theme, cx);

        div()
            .w_full()
            .v_flex()
            .gap(px(16.))
            .child(header)
            .child(device_card)
            .child(policy_heading)
            .child(policy_card)
            .child(add_toggle)
            .when_some(add_panel, gpui::ParentElement::child)
            .child(policy_text_heading)
            .child(policy_text_card)
            .child(add_txt_toggle)
            .when_some(add_txt_panel, gpui::ParentElement::child)
            .child(plugins_heading)
            .when_some(add_plugin_panel, gpui::ParentElement::child)
            .child(plugins_card)
            .child(remote_heading)
            .child(remote_card)
            .when_some(reset_history, gpui::ParentElement::child)
    }
}

fn render_device_header(tray_resident: bool, theme: &Theme) -> AnyElement {
    div()
        .h_flex()
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
                Tone::Danger.icon(Icon::Close),
                theme,
                |_, cx| cx.quit(),
            ))
        })
        .into_any_element()
}

fn settings_form(theme: &Theme) -> gpui::Div {
    surface_card(theme).p(px(16.)).v_flex().gap(px(10.))
}

fn section_title(title: &'static str, theme: &Theme) -> gpui::Div {
    div()
        .text_size(px(18.))
        .font_weight(FontWeight::BOLD)
        .text_color(theme.ink)
        .child(title)
}

fn section_intro(title: &'static str, description: &'static str, theme: &Theme) -> gpui::Div {
    div()
        .v_flex()
        .gap(px(4.))
        .child(section_title(title, theme))
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child(description),
        )
}

fn render_history_reset_card(
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> Option<AnyElement> {
    cx.try_global::<super::HistoryReset>().cloned().map(|reset| {
        surface_card(theme)
            .child(row_frame(
                false,
                "会話・作業の履歴と記憶",
                None,
                pill_button("dev-reset-history", "履歴をリセット", Tone::Danger.icon(Icon::Trash).enabled(live), theme, move |window, cx| {
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
                                cx.update(|cx| cx.quit());
                            }
                        })
                        .detach();
                    }),
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
    })
}

pub(super) fn row_frame(
    top_border: bool,
    title: &'static str,
    status: Option<AnyElement>,
    control: AnyElement,
    theme: &Theme,
) -> AnyElement {
    div()
        .w_full()
        .when(top_border, |this| {
            this.border_t_1().border_color(theme.border)
        })
        .px(px(16.))
        .py(px(14.))
        .h_flex()
        .justify_between()
        .gap(px(12.))
        .child(
            div()
                .h_flex()
                .gap(px(10.))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.ink)
                        .child(title),
                )
                .when_some(status, gpui::ParentElement::child),
        )
        .child(control)
        .into_any_element()
}

pub(super) fn device_pill(device: &Device, active_flag: bool, theme: &Theme) -> AnyElement {
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
    use ene_client_core::domain::PolicyEffect;

    #[test]
    fn tool_confirmation_updates_only_managed_rules() {
        let enabled = tool_confirmation_request(true);
        let disabled = tool_confirmation_request(false);
        assert_eq!(
            disabled.add_or_update,
            [] as [ene_client_core::domain::PolicyRule; 0]
        );
        let [rule] = enabled.add_or_update.as_slice() else {
            panic!("expected one global rule")
        };
        assert_eq!(rule.id, SKIP_TOOL_CONFIRMATION_ID);
        assert_eq!(rule.tool_name, "*");
        assert_eq!(rule.pattern, "*");
        assert_eq!(rule.effect, PolicyEffect::Allow);
        assert_eq!(enabled.remove_ids.len(), WORKSPACE_FILE_CAPABILITIES.len());
        assert!(
            enabled
                .remove_ids
                .iter()
                .all(|id| is_file_confirmation_rule(&PolicyRule {
                    id: id.clone(),
                    ..rule.clone()
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
        assert!(!is_file_confirmation_rule(&PolicyRule {
            id: "manual-write-rule".into(),
            tool_name: "work.write".into(),
            ..rule.clone()
        }));
    }
}
