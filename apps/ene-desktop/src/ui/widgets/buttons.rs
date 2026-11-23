//! Buttons: the pill button, the floating orb and the toggle switch.

//! Reusable GPUI UI primitives: capsule pills, detached orbs, toggle switches,
//! companion emblems and progressive disclosure controls.

use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use gpui::{
    AnyElement, ElementId, Hsla, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};

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
    let (bg, fg, border) = style.tone.colors(style.enabled, theme);
    let hover_bg = match style.tone {
        Tone::Primary => theme.accent_hover,
        Tone::Ghost => theme.bg_blush,
        _ => bg.alpha(0.84),
    };
    let mut el = div()
        .id(id)
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(14.))
        .py(px(6.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(gpui::FontWeight::BOLD)
        .when_some(border, |this, color| this.border_1().border_color(color));
    if let Some(kind) = style.leading {
        el = el.child(icon(kind, 13., fg));
    }
    el = el.child(label.into());
    if style.enabled {
        el.cursor_pointer()
            .hover(move |this| this.bg(hover_bg))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_press(window, cx);
            })
            .into_any_element()
    } else {
        el.into_any_element()
    }
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
    div()
        .id(id)
        .flex_none()
        .w(px(40.))
        .h(px(40.))
        .rounded_full()
        .bg(bg)
        .border_2()
        .border_color(border)
        .shadow(theme.float_shadow())
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(move |this| this.bg(hover_bg))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            on_press(window, cx);
        })
        .child(icon(kind, 16., fg))
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
}
/// Compact circular icon button inside inputs or rows.
pub fn circle_icon_button<F>(
    id: impl Into<ElementId>,
    style: CircleButtonStyle,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let (bg, fg, border) = style.tone.colors(style.enabled, theme);
    let hover_bg = match style.tone {
        Tone::Primary => theme.accent_hover,
        Tone::Ghost => theme.bg_blush,
        _ => bg.alpha(0.84),
    };
    let el = div()
        .id(id)
        .flex_none()
        .w(px(style.button_size))
        .h(px(style.button_size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(bg)
        .when_some(border, |this, color| this.border_1().border_color(color))
        .child(icon(style.icon, style.icon_size, fg));
    if style.enabled {
        el.cursor_pointer()
            .hover(move |this| this.bg(hover_bg))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_press(window, cx);
            })
            .into_any_element()
    } else {
        el.into_any_element()
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
    let track_bg = if !enabled {
        theme.border.alpha(0.7)
    } else if checked {
        theme.mint
    } else {
        theme.border_float
    };
    let thumb = div()
        .size(px(16.))
        .rounded_full()
        .bg(theme.bg_surface)
        .when(!enabled, |this| {
            this.border_1().border_color(theme.border_float)
        });
    let mut track = div()
        .id(id)
        .flex_none()
        .w(px(40.))
        .h(px(22.))
        .px(px(3.))
        .rounded_full()
        .bg(track_bg)
        .when(!enabled, |this| {
            this.border_1().border_color(theme.border_float)
        })
        .flex()
        .items_center()
        .when(checked, |this| this.justify_end())
        .when(!checked, |this| this.justify_start())
        .child(thumb);
    if enabled {
        track = track
            .cursor_pointer()
            .hover(move |this| this.bg(track_bg.alpha(0.86)))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_toggle(window, cx);
            });
    }
    track.into_any_element()
}
