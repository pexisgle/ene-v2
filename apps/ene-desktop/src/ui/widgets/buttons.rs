//! App palette and icon styles for GPUI Kit buttons and switches.

use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use gpui::component::button::{Button, ButtonCustomVariant, ButtonVariants};
use gpui::component::switch::Switch;
use gpui::component::{Disableable, Selectable, Sizable};
use gpui::{AnyElement, App, ElementId, Hsla, SharedString, Window, prelude::*, px};

/// Semantic tone for capsule buttons and tags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Soft,
    Mint,
    Ghost,
    Danger,
}

impl Tone {
    pub const fn icon(self, leading: Icon) -> ButtonStyle {
        ButtonStyle::new(self).icon(leading)
    }

    pub const fn maybe_icon(self, leading: Option<Icon>) -> ButtonStyle {
        ButtonStyle::new(self).maybe_icon(leading)
    }

    pub const fn enabled(self, enabled: bool) -> ButtonStyle {
        ButtonStyle::new(self).enabled(enabled)
    }

    fn hover(self, bg: Hsla, theme: &Theme) -> Hsla {
        match self {
            Self::Primary => theme.accent_hover,
            Self::Ghost => theme.bg_blush,
            _ => bg.alpha(0.84),
        }
    }

    fn colors(self, enabled: bool, theme: &Theme) -> (Hsla, Hsla, Option<Hsla>) {
        if !enabled {
            return (theme.bg_blush, theme.ink_placeholder, Some(theme.border));
        }
        match self {
            Self::Primary => (theme.accent, theme.accent_fg, None),
            Self::Soft => (theme.accent_soft, theme.accent_soft_fg, Some(theme.border)),
            Self::Mint => (theme.mint_soft, theme.mint_fg, None),
            Self::Ghost => (theme.bg_surface, theme.ink_muted, Some(theme.border)),
            Self::Danger => (
                theme.danger_soft,
                theme.danger_fg,
                Some(theme.danger.alpha(0.35)),
            ),
        }
    }
}
/// Visual state and optional icon configuration for [`pill_button`].
#[derive(Clone, Copy, Debug)]
pub struct ButtonStyle {
    pub leading: Option<Icon>,
    pub tone: Tone,
    pub enabled: bool,
}
impl ButtonStyle {
    pub const fn new(tone: Tone) -> Self {
        Self {
            leading: None,
            tone,
            enabled: true,
        }
    }

    pub const fn icon(mut self, leading: Icon) -> Self {
        self.leading = Some(leading);
        self
    }

    pub const fn maybe_icon(mut self, leading: Option<Icon>) -> Self {
        self.leading = leading;
        self
    }

    pub const fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}
impl From<Tone> for ButtonStyle {
    fn from(tone: Tone) -> Self {
        Self::new(tone)
    }
}
/// Tactile capsule button with optional leading SVG icon.
pub fn pill_button<F>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    style: impl Into<ButtonStyle>,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let style = style.into();
    let label = label.into();
    let (bg, fg, border) = style.tone.colors(style.enabled, theme);
    ThemedButton {
        button: Button::new(id)
            .rounded(px(999.))
            .disabled(!style.enabled)
            .h_auto()
            .px(px(14.))
            .py(px(6.))
            .text_xs()
            .font_weight(gpui::FontWeight::BOLD)
            .gap(px(6.))
            .accessibility_label(label.clone())
            .when_some(style.leading, |button, kind| {
                button.child(icon(kind, 13., fg))
            })
            .child(label)
            .on_click(move |_, window, cx| on_press(window, cx)),
        bg,
        fg,
        border,
        hover: style.tone.hover(bg, theme),
    }
    .into_any_element()
}

/// A keyboard-accessible chapter button in the settings palette.
pub(crate) fn navigation_button(
    id: &'static str,
    label: &'static str,
    leading: Icon,
    active: bool,
    theme: &Theme,
    activate: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let (bg, fg, icon_fg, hover) = if active {
        (theme.accent, theme.accent_fg, theme.accent_fg, theme.accent)
    } else {
        (
            gpui::transparent_black(),
            theme.ink,
            theme.ink_muted,
            theme.bg_surface.alpha(0.65),
        )
    };
    ThemedButton {
        button: Button::new(id)
            .w_full()
            .h_auto()
            .px(px(14.))
            .py(px(9.))
            .rounded(px(999.))
            .text_sm()
            .font_weight(if active {
                gpui::FontWeight::BOLD
            } else {
                gpui::FontWeight::MEDIUM
            })
            .selected(active)
            .accessibility_label(label)
            .child(
                gpui::div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(icon(leading, 15., icon_fg))
                    .child(label),
            )
            .on_click(move |_, window, cx| activate(window, cx)),
        bg,
        fg,
        border: None,
        hover,
    }
    .into_any_element()
}

/// Detached circular orb button used next to the floating capsule dock.
pub fn floating_orb_button<F>(
    id: impl Into<ElementId>,
    kind: Icon,
    active: bool,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let (bg, fg, border, hover_bg) = if active {
        (
            theme.accent,
            theme.accent_fg,
            theme.accent,
            theme.accent_hover,
        )
    } else {
        (
            theme.bg_surface,
            theme.ink_muted,
            theme.border_float,
            theme.bg_blush,
        )
    };
    ThemedButton {
        button: Button::new(id)
            .size(px(40.))
            .rounded(px(20.))
            .p_0()
            .shadow(theme.float_shadow())
            .child(icon(kind, 16., fg))
            .on_click(move |_, window, cx| on_press(window, cx)),
        bg,
        fg,
        border: Some(border),
        hover: hover_bg,
    }
    .into_any_element()
}

/// Configuration for [`circle_icon_button`].
#[derive(Clone, Copy, Debug)]
pub struct CircleButtonStyle {
    pub icon: Icon,
    pub button_size: f32,
    pub icon_size: f32,
    pub tone: Tone,
    pub enabled: bool,
}
impl CircleButtonStyle {
    pub const fn new(icon: Icon, button_size: f32, icon_size: f32) -> Self {
        Self {
            icon,
            button_size,
            icon_size,
            tone: Tone::Ghost,
            enabled: true,
        }
    }

    pub const fn tone(mut self, tone: Tone) -> Self {
        self.tone = tone;
        self
    }

    pub const fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}
/// Compact circular icon button inside inputs or rows.
pub fn circle_icon_button<F>(
    id: impl Into<ElementId>,
    label: &'static str,
    style: CircleButtonStyle,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let (bg, fg, border) = style.tone.colors(style.enabled, theme);
    ThemedButton {
        button: Button::new(id)
            .flex_none()
            .size(px(style.button_size))
            .rounded(px(style.button_size / 2.))
            .p_0()
            .disabled(!style.enabled)
            .accessibility_label(label)
            .child(icon(style.icon, style.icon_size, fg))
            .on_click(move |_, window, cx| on_press(window, cx)),
        bg,
        fg,
        border,
        hover: style.tone.hover(bg, theme),
    }
    .into_any_element()
}

#[derive(IntoElement)]
struct ThemedButton {
    button: Button,
    bg: Hsla,
    fg: Hsla,
    hover: Hsla,
    border: Option<Hsla>,
}

impl RenderOnce for ThemedButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        self.button
            .custom(
                ButtonCustomVariant::new(cx)
                    .color(self.bg)
                    .foreground(self.fg)
                    .hover(self.hover)
                    .active(self.hover.alpha(0.88)),
            )
            .bg(self.bg)
            .text_color(self.fg)
            .when_some(self.border, |button, color| {
                button.border_1().border_color(color)
            })
    }
}

/// Plump capsule toggle switch.
pub fn toggle_switch<F>(
    id: impl Into<ElementId>,
    checked: bool,
    enabled: bool,
    theme: &Theme,
    on_toggle: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    Switch::new(id)
        .large()
        .color(theme.mint)
        .checked(checked)
        .disabled(!enabled)
        .on_change(move |_, window, cx| on_toggle(window, cx))
        .into_any_element()
}
