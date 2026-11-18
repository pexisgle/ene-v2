//! Reusable GPUI UI primitives: capsule pills, detached orbs, toggle switches,
//! companion emblems and progressive disclosure controls.

use super::icons::{Icon, icon};
use super::theme::{Theme, companion_palette};
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

/// Tactile capsule button with optional leading SVG icon.
#[allow(clippy::too_many_arguments)]
pub fn pill_button<F>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    leading: Option<Icon>,
    tone: Tone,
    enabled: bool,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let (bg, fg, border) = tone.colors(enabled, theme);
    let hover_bg = match tone {
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
    if let Some(kind) = leading {
        el = el.child(icon(kind, 13., fg));
    }
    el = el.child(label.into());
    if enabled {
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
    danger: bool,
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
    } else if danger {
        (
            theme.bg_surface,
            theme.ink_muted,
            theme.border_float,
            theme.danger_soft,
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

/// Compact circular icon button inside inputs or rows.
#[allow(clippy::too_many_arguments)]
pub fn circle_icon_button<F>(
    id: impl Into<ElementId>,
    kind: Icon,
    button_size: f32,
    icon_size: f32,
    tone: Tone,
    enabled: bool,
    theme: &Theme,
    on_press: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let (bg, fg, border) = tone.colors(enabled, theme);
    let hover_bg = match tone {
        Tone::Primary => theme.accent_hover,
        Tone::Ghost => theme.bg_blush,
        _ => bg.alpha(0.84),
    };
    let el = div()
        .id(id)
        .flex_none()
        .w(px(button_size))
        .h(px(button_size))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(bg)
        .when_some(border, |this, color| this.border_1().border_color(color))
        .child(icon(kind, icon_size, fg));
    if enabled {
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
        theme.bg_blush
    } else if checked {
        theme.mint
    } else {
        theme.border_float
    };
    let thumb = div().size(px(16.)).rounded_full().bg(theme.bg_surface);
    let mut track = div()
        .id(id)
        .flex_none()
        .w(px(40.))
        .h(px(22.))
        .px(px(3.))
        .rounded_full()
        .bg(track_bg)
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

/// Status pill with an optional live dot indicator.
pub fn status_pill(
    label: impl Into<SharedString>,
    dot: Option<Hsla>,
    bg: Hsla,
    fg: Hsla,
) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(5.))
        .px(px(10.))
        .py(px(3.))
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .font_weight(gpui::FontWeight::BOLD)
        .when_some(dot, |this, color| {
            this.child(div().size(px(6.)).rounded_full().bg(color))
        })
        .child(label.into())
        .into_any_element()
}

/// Strawberry-milk tag pill for connection/capability kinds.
pub fn kind_pill(label: impl Into<SharedString>, theme: &Theme) -> AnyElement {
    div()
        .flex_none()
        .px(px(10.))
        .py(px(3.))
        .rounded_full()
        .bg(theme.bg_milk)
        .text_color(theme.ink_muted)
        .text_xs()
        .font_weight(gpui::FontWeight::BOLD)
        .child(label.into())
        .into_any_element()
}

/// Companion pastel emblem circle with initial and optional status dot.
pub fn companion_badge(
    name: &str,
    id: &str,
    badge_size: f32,
    active_dot: Option<Hsla>,
    theme: &Theme,
) -> AnyElement {
    let palette = companion_palette(if id.is_empty() { name } else { id });
    let initial: String = name
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().collect())
        .unwrap_or_else(|| "e".to_owned());
    let font_px = (badge_size * 0.44).max(11.);
    let dot_px = (badge_size * 0.28).clamp(7., 10.);
    div()
        .relative()
        .flex_none()
        .w(px(badge_size))
        .h(px(badge_size))
        .child(
            div()
                .size_full()
                .rounded_full()
                .bg(palette.bg)
                .border_1()
                .border_color(palette.ring)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(font_px))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(palette.fg)
                        .child(initial),
                ),
        )
        .when_some(active_dot, |this, dot_color| {
            this.child(
                div()
                    .absolute()
                    .bottom(px(-1.))
                    .right(px(-1.))
                    .size(px(dot_px))
                    .rounded_full()
                    .bg(dot_color)
                    .border_2()
                    .border_color(theme.bg_surface),
            )
        })
        .into_any_element()
}

/// Progressive disclosure toggle line using SVG chevron icons.
pub fn disclosure_row<F>(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    open: bool,
    theme: &Theme,
    on_toggle: F,
) -> AnyElement
where
    F: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let chevron = if open {
        Icon::ChevronDown
    } else {
        Icon::ChevronRight
    };
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.))
        .py(px(6.))
        .cursor_pointer()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.ink_muted)
        .hover(|this| this.text_color(theme.ink))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            on_toggle(window, cx);
        })
        .child(icon(chevron, 14., theme.ink_muted))
        .child(label.into())
        .into_any_element()
}
