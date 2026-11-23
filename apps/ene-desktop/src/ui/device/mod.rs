//! `この端末` chapter in `SettingsStudio`: active client handoff, mic/speaker toggles,
//! 3D avatar scale & re-center, capability tags, plugins, remote host, and policy rules.

mod hardware;
mod plugins;
mod policy;

use super::icons::Icon;
use super::theme::Theme;
use super::widgets::{Tone, pill_button, status_pill};
use super::{DraftStore, Hub};
use crate::WORKSPACE_FILE_CAPABILITIES;
use crate::state::{Device, Link};
#[cfg(windows)]
use ene_client_core::domain::COMPUTER_USE;
use ene_client_core::domain::{
    ConnectionKind, PluginInfo, PolicyEffect, PolicyRule, PolicyRulesUpdate, PolicyText,
    SKIP_TOOL_CONFIRMATION_ID, is_file_confirmation_rule as domain_is_file_confirmation,
    tool_confirmation_update,
};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px};

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
            skip_confirm_armed: false,
        }
    }
}

/// Device, plugin, remote-host, and policy chapter rendered as an independent GPUI pane entity.
pub(crate) struct DevicePane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    pub(super) form: DeviceFormState,
    _observe: Subscription,
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

pub(super) struct DeviceSnapshot {
    pub(super) live: bool,
    pub(super) link: Link,
    pub(super) is_active_client: bool,
    pub(super) has_companion: bool,
    pub(super) devices: crate::state::Devices,
    pub(super) mic_muted: bool,
    pub(super) output_muted: bool,
    pub(super) mic_configured: bool,
    pub(super) output_configured: bool,
    pub(super) eyes_enabled: bool,
    pub(super) eyes_status: Option<String>,
    pub(super) avatar_scale: f32,
    pub(super) rules: Vec<PolicyRule>,
    pub(super) policy_texts: Vec<PolicyText>,
    pub(super) client_caps: Vec<String>,
    pub(super) plugins: Vec<PluginInfo>,
    pub(super) tray_resident: bool,
    pub(super) pod_visible: bool,
    #[cfg(windows)]
    pub(super) computer_use: bool,
    #[cfg(windows)]
    pub(super) computer_granted: bool,
}

impl DeviceSnapshot {
    fn capture(hub: &Hub) -> Self {
        let our_id = &hub.options.id;
        let client = hub
            .model
            .snapshot()
            .and_then(|s| s.clients.iter().find(|c| c.id == *our_id));
        #[cfg(windows)]
        let computer_granted = client.is_some_and(|c| {
            c.granted_capabilities
                .iter()
                .any(|name| name == COMPUTER_USE)
        });
        Self {
            live: hub.live(),
            link: hub.model.connections.link.clone(),
            is_active_client: hub.model.is_active_client(our_id),
            has_companion: hub.model.companion_id().is_some(),
            devices: hub.model.connections.devices.clone(),
            mic_muted: hub.model.audio.mic_muted,
            output_muted: hub.model.audio.output_muted,
            mic_configured: hub.has_audio_connection(ConnectionKind::Stt),
            output_configured: hub.has_audio_connection(ConnectionKind::Tts),
            eyes_enabled: hub.model.perception.eyes_enabled,
            eyes_status: hub.model.perception.eyes_status.clone(),
            avatar_scale: hub.model.perception.avatar_scale,
            rules: hub
                .model
                .snapshot()
                .map(|s| s.policy_rules.clone())
                .unwrap_or_default(),
            policy_texts: hub.model.policy_texts().to_vec(),
            client_caps: client.map(|c| c.capabilities.clone()).unwrap_or_default(),
            plugins: hub.model.connections.plugins.clone(),
            tray_resident: hub.is_tray_resident(),
            pod_visible: hub.is_pod_visible(),
            #[cfg(windows)]
            computer_use: hub.model.perception.computer_use_enabled,
            #[cfg(windows)]
            computer_granted,
        }
    }
}

impl DevicePane {
    pub(crate) fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            drafts: DraftStore::default(),
            form: DeviceFormState::default(),
            _observe,
        }
    }

    pub(crate) fn on_leave(&mut self) {
        self.form.skip_confirm_armed = false;
    }
}

impl gpui::Render for DevicePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, snap) = {
            let hub = self.hub.read(cx);
            (hub.theme(), DeviceSnapshot::capture(hub))
        };
        let header = render_device_header(snap.tray_resident, &theme);
        let device_card = hardware::render_device_status_card(self, &snap, &theme);
        let (policy_heading, policy_card, add_toggle, add_panel) =
            policy::render_policy_rules_section(self, &snap, &theme, cx);
        let (policy_text_heading, policy_text_card, add_txt_toggle, add_txt_panel) =
            policy::render_policy_texts_section(self, &snap, &theme, cx);
        let (plugins_heading, add_plugin_panel, plugins_card) =
            plugins::render_plugins_section(self, &snap, &theme, cx);
        let (remote_heading, remote_card) = plugins::render_remote_host_section(self, &theme, cx);
        let reset_history = render_history_reset_card(snap.live, &theme, cx);

        div()
            .w_full()
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
            .when_some(add_plugin_panel, |this, panel| this.child(panel))
            .child(plugins_card)
            .child(remote_heading)
            .child(remote_card)
            .when_some(reset_history, |this, card| this.child(card))
    }
}

fn render_device_header(tray_resident: bool, theme: &Theme) -> AnyElement {
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

fn render_history_reset_card(
    live: bool,
    theme: &Theme,
    cx: &mut Context<DevicePane>,
) -> Option<AnyElement> {
    cx.try_global::<super::HistoryReset>().cloned().map(|reset| {
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
                                let _discarded =  cx.update(|cx| cx.quit());
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
        assert!(disabled.add_or_update.is_empty());
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
        assert!(!is_file_confirmation_rule(&PolicyRule {
            id: "manual-write-rule".into(),
            tool_name: "work.write".into(),
            ..Default::default()
        }));
    }
}
