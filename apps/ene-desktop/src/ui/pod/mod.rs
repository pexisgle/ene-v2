//! Minimal frameless floating companion window: speech bubble, inline approval,
//! first-run onboarding, collapsible history, capsule composer and controls menu.

mod bubble;
mod controls;
mod menu;
mod onboarding;

use crate::input::{self, Draft};
use crate::ui::theme::{resolve_ui_font_family, ui_font_fallbacks};
use crate::ui::{Hub, PodMode};
use ene_client_core::domain::TurnStatus;
use gpui::{
    App, Bounds, Context, Entity, KeyBinding, Pixels, ScrollHandle, Size, Subscription, Window,
    actions, div, point, prelude::*, px,
};

const MAX_INPUT: usize = 4000;

actions!(
    ene_desktop,
    [
        Submit,
        CancelOrStop,
        ToggleMicShortcut,
        OpenSettingsShortcut
    ]
);

/// Compatibility alias expected by `lib.rs`.
pub type Desktop = CompanionPod;

/// Computes default bottom-right floating placement on the primary display.
pub fn initial_pod_bounds(cx: &App) -> Bounds<Pixels> {
    initial_pod_bounds_for_mode(PodMode::MiniVoice, cx)
}

/// Computes bottom-right placement sized for the requested pod mode.
pub fn initial_pod_bounds_for_mode(mode: PodMode, cx: &App) -> Bounds<Pixels> {
    let size = mode.size();
    if let Some(display) = cx.primary_display() {
        let screen = display.bounds();
        let x = (screen.right() - size.width - px(28.)).max(screen.left() + px(16.));
        let y = (screen.bottom() - size.height - px(64.)).max(screen.top() + px(16.));
        Bounds::new(point(x, y), size)
    } else {
        Bounds::centered(None, size, cx)
    }
}

/// Frameless transparent companion dialogue window.
pub struct CompanionPod {
    pub(super) hub: Entity<Hub>,
    pub(super) chat_input: Entity<Draft>,
    pub(super) onboard_key: Entity<Draft>,
    pub(super) onboard_name: Entity<Draft>,
    pub(super) setup_model_id: Option<String>,
    pub(super) setup_model_dropdown_open: bool,
    pub(super) setup_model_filter: Entity<Draft>,
    pub(super) setup_voice_speaker_id: Option<String>,
    pub(super) setup_voice_dropdown_open: bool,
    pub(super) setup_voice_filter: Entity<Draft>,
    pub(super) selected_provider_id: Option<String>,
    pub(super) choosing_provider: bool,
    key_focused: bool,
    pub(super) roster_open: bool,
    pub(super) controls_open: bool,
    /// Whether the approval card's raw arguments are expanded.
    pub(super) approval_args_open: bool,
    last_size: Size<Pixels>,
    title_initialized: bool,
    pub(super) scroll: ScrollHandle,
    last_scroll_ping: u64,
    _observe: Subscription,
}

impl std::fmt::Debug for CompanionPod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionPod")
            .field("roster_open", &self.roster_open)
            .field("controls_open", &self.controls_open)
            .finish_non_exhaustive()
    }
}

impl CompanionPod {
    pub fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let chat_input = cx.new(|cx| Draft::new(cx, "メッセージを入力…", "ChatInput"));
        let onboard_key = cx.new(|cx| {
            let mut draft = Draft::new(cx, "APIキーを入力…", "TextField");
            draft.set_masked(true, cx);
            draft
        });
        let onboard_name = cx.new(|cx| {
            let mut draft = Draft::new(cx, "コンパニオン名", "TextField");
            draft.set("ene", cx);
            draft
        });
        let setup_model_filter = cx.new(|cx| Draft::new(cx, "モデルを検索…", "TextField"));
        let setup_voice_filter = cx.new(|cx| Draft::new(cx, "話者を検索…", "TextField"));
        let _observe = cx.observe(&hub, |_, _, cx| cx.notify());
        Self {
            hub,
            chat_input,
            onboard_key,
            onboard_name,
            setup_model_id: None,
            setup_model_dropdown_open: false,
            setup_model_filter,
            setup_voice_speaker_id: None,
            setup_voice_dropdown_open: false,
            setup_voice_filter,
            selected_provider_id: None,
            choosing_provider: false,
            key_focused: false,
            roster_open: false,
            controls_open: false,
            approval_args_open: false,
            last_size: PodMode::MiniVoice.size(),
            title_initialized: false,
            scroll: ScrollHandle::new(),
            last_scroll_ping: 0,
            _observe,
        }
    }

    pub(super) fn needs_connection(&self, cx: &App) -> bool {
        let hub = self.hub.read(cx);
        hub.live()
            && hub.model.settings().is_some_and(|settings| {
                hub.setup.pending
                    || hub.setup.error.is_some()
                    || hub
                        .model
                        .connections
                        .providers
                        .as_deref()
                        .is_none_or(|providers| {
                            !settings
                                .connections
                                .iter()
                                .any(|c| c.can_start_chat(providers))
                        })
            })
    }

    pub(super) fn in_model_setup(&self, cx: &App) -> bool {
        let hub = self.hub.read(cx);
        if !hub.live() {
            return false;
        }
        let companions_empty = hub
            .model
            .snapshot()
            .is_none_or(|snap| snap.companions.is_empty());
        hub.setup.pending_connection_id.is_some() || companions_empty
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut App) {
        let field = if self.needs_connection(cx) {
            &self.onboard_key
        } else if self.in_model_setup(cx) {
            let companions_empty = self
                .hub
                .read(cx)
                .model
                .snapshot()
                .is_none_or(|snap| snap.companions.is_empty());
            if companions_empty {
                &self.onboard_name
            } else {
                &self.setup_model_filter
            }
        } else {
            &self.chat_input
        };
        let handle = field.read(cx).focus_handle();
        window.focus(&handle);
    }

    pub(crate) fn sync_mode_and_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let desired = self.desired_size(cx);
        self.apply_size(desired, window, cx);
        self.focus_input(window, cx);
        ensure_pod_topmost();
    }

    pub(super) fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.hub.read(cx).windows.pod_mode.toggle();
        self.hub.update(cx, |hub, cx| hub.set_pod_mode(next, cx));
        let desired = self.desired_size(cx);
        self.apply_size(desired, window, cx);
        cx.notify();
    }

    fn desired_size(&self, cx: &App) -> Size<Pixels> {
        let hub = self.hub.read(cx);
        let needs_conn = self.needs_connection(cx);
        if needs_conn || self.in_model_setup(cx) {
            return self.onboarding_size(hub, needs_conn);
        }
        if hub.windows.pod_mode == PodMode::ExpandedChat {
            return PodMode::ExpandedChat.size();
        }
        if !hub.model.approvals().is_empty() || self.roster_open {
            return gpui::size(px(444.), px(376.));
        }
        mini_bubble_size(&hub.model)
    }

    fn onboarding_size(&self, hub: &Hub, needs_conn: bool) -> Size<Pixels> {
        let base = if needs_conn {
            if hub.setup.error_detail.is_some() {
                520.
            } else {
                496.
            }
        } else if self.setup_model_dropdown_open || self.setup_voice_dropdown_open {
            580.
        } else {
            492.
        };
        let h = if hub.model.notices.is_empty() {
            base
        } else {
            base + 44.
        };
        gpui::size(px(444.), px(h))
    }

    fn apply_size(&mut self, desired: Size<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.last_size != desired {
            #[cfg(windows)]
            let saved_bottom = ene_platform_os::PodWindow::current()
                .and_then(|pod| pod.bottom().map(|bottom| (pod, bottom)));
            self.last_size = desired;
            window.resize(desired);
            #[cfg(windows)]
            if let Some((pod, target_bottom)) = saved_bottom {
                cx.spawn(async move |_, _| {
                    pod.anchor_bottom(target_bottom);
                })
                .detach();
            }
            #[cfg(not(windows))]
            let _cx = cx;
        }
    }

    pub(super) fn submit(&mut self, cx: &mut Context<Self>) {
        if self.needs_connection(cx)
            || self.in_model_setup(cx)
            || self.chat_input.read(cx).composing()
        {
            return;
        }
        let text = self.chat_input.read(cx).text().trim().to_owned();
        if text.is_empty() || text.chars().count() > MAX_INPUT {
            return;
        }
        let sent = self.hub.update(cx, |hub, cx| hub.send(text, cx));
        if sent {
            self.controls_open = false;
            self.chat_input
                .update(cx, super::super::input::Draft::clear);
        }
    }

    fn handle_escape(&mut self, cx: &mut Context<Self>) {
        if self.controls_open {
            self.controls_open = false;
            cx.notify();
            return;
        }
        if self.setup_model_dropdown_open || self.setup_voice_dropdown_open {
            self.setup_model_dropdown_open = false;
            self.setup_voice_dropdown_open = false;
            cx.notify();
            return;
        }
        if self.roster_open {
            self.roster_open = false;
            cx.notify();
            return;
        }
        let hub = self.hub.read(cx);
        let busy = hub.model.audio.companion_speaking || !hub.model.companions.streaming.is_empty();
        let expanded = hub.windows.pod_mode == PodMode::ExpandedChat;
        if busy {
            self.hub.update(cx, super::hub::Hub::stop);
        } else if expanded {
            self.hub
                .update(cx, |hub, cx| hub.set_pod_mode(PodMode::MiniVoice, cx));
        }
    }
}

fn mini_bubble_size(model: &crate::state::Model) -> Size<Pixels> {
    let speech_text = bubble::active_mini_speech_text(model);
    let estimated_lines = speech_text
        .lines()
        .map(|line| line.chars().count().max(1).div_ceil(24))
        .sum::<usize>()
        .clamp(2, 7);
    let mut h = 152.0 + (estimated_lines as f32) * 20.0;
    if estimated_lines > 6 || model.companions.chat.messages.is_empty() {
        h += 40.0;
    }
    if !bubble::extract_urls(&speech_text).is_empty() {
        h += 28.0;
    }
    if model.audio.transcription.is_some() {
        h += 38.0;
    }
    if !model.notices.is_empty() {
        h += 46.0;
    }
    let turn_failed = model
        .companions
        .chat
        .turn
        .as_ref()
        .is_some_and(|t| t.status == TurnStatus::Failed);
    if model.audio.companion_speaking
        || !model.companions.streaming.is_empty()
        || model.companions.chat.stop_requested
        || turn_failed
    {
        h += 36.0;
    }
    gpui::size(px(444.), px(h.clamp(188., 400.)))
}

impl Render for CompanionPod {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.title_initialized {
            self.title_initialized = true;
            window.set_window_title("ene-companion-pod");
            #[cfg(windows)]
            if let Some(pod) = ene_platform_os::PodWindow::current() {
                pod.suppress_dwm_frame();
                pod.keep_topmost();
            }
        }
        let needs_key = self.needs_connection(cx);
        let in_onboarding = needs_key || self.in_model_setup(cx);
        if in_onboarding != self.key_focused {
            self.key_focused = in_onboarding;
            self.controls_open = false;
            self.focus_input(window, cx);
        }
        let desired = self.desired_size(cx);
        let (scroll_ping, pod_visible, theme) = {
            let hub = self.hub.read(cx);
            (
                hub.windows.pod_scroll_ping,
                hub.is_pod_visible(),
                hub.theme(),
            )
        };
        self.apply_size(desired, window, cx);
        if pod_visible {
            ensure_pod_topmost();
        }
        if scroll_ping != self.last_scroll_ping {
            self.last_scroll_ping = scroll_ping;
            self.scroll.scroll_to_bottom();
        }

        let roster_open = self.roster_open;

        div()
            .key_context("CompanionPod")
            .relative()
            .size_full()
            .bg(gpui::transparent_black())
            .font({
                let mut f = gpui::font(resolve_ui_font_family(cx));
                f.fallbacks = Some(ui_font_fallbacks());
                f
            })
            .text_color(theme.ink)
            .p(px(14.))
            .flex()
            .flex_col()
            .justify_end()
            .gap(px(10.))
            .child(self.render_bubble_stage(cx))
            .when(!in_onboarding, |root| {
                root.child(controls::render_capsule_dock(self, cx))
            })
            .when(roster_open, |this| {
                this.child(controls::render_roster_popover(self, &theme, cx))
            })
    }
}

/// The pod's shortcuts. Every one is scoped to a key context: an unscoped
/// `Escape` would also cancel a pod turn while `SettingsStudio` has focus.
fn pod_key_bindings() -> [KeyBinding; 4] {
    [
        KeyBinding::new("enter", Submit, Some("ChatInput")),
        KeyBinding::new("escape", CancelOrStop, Some(POD_KEY_CONTEXT)),
        // Scoped to the pod, so Ctrl/Cmd+M types an "m" in a settings field
        // rather than muting the microphone under it.
        KeyBinding::new("secondary-m", ToggleMicShortcut, Some(POD_KEY_CONTEXT)),
        KeyBinding::new("secondary-,", OpenSettingsShortcut, Some(POD_KEY_CONTEXT)),
    ]
}

/// The key context the pod root declares with `.key_context(..)`.
const POD_KEY_CONTEXT: &str = "CompanionPod";

/// Binds keyboard shortcuts (`Enter` send in `ChatInput`, `Esc` stop/collapse) globally.
pub fn bind(cx: &mut App, hub: Entity<Hub>) {
    input::bind(cx);
    cx.bind_keys(pod_key_bindings());

    let submit_hub = hub.clone();
    cx.on_action(move |_: &Submit, cx: &mut App| {
        let Some(view) = submit_hub.read(cx).pod_view() else {
            return;
        };
        let _updated = view.update(cx, CompanionPod::submit);
    });

    let escape_hub = hub.clone();
    cx.on_action(move |_: &CancelOrStop, cx: &mut App| {
        let Some(view) = escape_hub.read(cx).pod_view() else {
            return;
        };
        let _updated = view.update(cx, CompanionPod::handle_escape);
    });

    let mic_hub = hub.clone();
    cx.on_action(move |_: &ToggleMicShortcut, cx: &mut App| {
        mic_hub.update(cx, super::hub::Hub::toggle_mic_mute);
    });

    cx.on_action(move |_: &OpenSettingsShortcut, cx: &mut App| {
        hub.update(cx, |hub, cx| {
            hub.open_settings(crate::ui::SettingsSection::Companions, cx);
        });
    });
}

/// Focuses the pod capsule input field when the window opens.
pub fn focus_chat(pod: &CompanionPod, window: &mut Window, cx: &mut App) {
    pod.focus_input(window, cx);
}

pub(crate) fn set_pod_window_visible(visible: bool) {
    #[cfg(windows)]
    if let Some(pod) = ene_platform_os::PodWindow::current() {
        pod.set_visible(visible);
    }
    #[cfg(not(windows))]
    let _visible = visible;
}

pub(crate) fn ensure_pod_topmost() {
    #[cfg(windows)]
    if let Some(pod) = ene_platform_os::PodWindow::current() {
        pod.keep_topmost();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pod shortcut is scoped to a key context. An unscoped `Escape`
    /// cancels a pod turn while `SettingsStudio` has focus, which is what this
    /// pins: the binding must name a context, and `Escape` must name the pod's.
    #[test]
    fn pod_shortcuts_are_scoped_to_a_key_context() {
        let bindings = pod_key_bindings();
        // No count: the point is that nothing is unscoped, and a count would
        // have to be edited every time a shortcut is added.
        for binding in &bindings {
            assert!(
                binding.predicate().is_some(),
                "an unscoped binding fires everywhere: {binding:?}"
            );
        }
        let scoped_to_pod = bindings
            .iter()
            .filter(|binding| {
                binding
                    .predicate()
                    .is_some_and(|predicate| predicate.to_string().contains(POD_KEY_CONTEXT))
            })
            .count();
        assert!(
            scoped_to_pod >= 3,
            "Escape, the mic toggle and settings are all pod-scoped, got {scoped_to_pod}: {bindings:?}"
        );
    }
}
