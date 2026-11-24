//! Pills, badges and the rows that hold them.

use crate::ui::icons::{Icon, icon};
use crate::ui::theme::{Theme, companion_palette};
use gpui::{
    AnyElement, ElementId, Hsla, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};

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
        .map_or_else(|| "e".to_owned(), |ch| ch.to_uppercase().collect());
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
/// Marks a container as a draggable window title/handle region across platforms.
///
/// Windows uses `WindowControlArea::Drag` in `WM_NCHITTEST`, while GPUI's
/// Wayland and X11 backends leave `on_hit_test_window_control` empty and expect
/// an explicit `window.start_window_move()` on left mouse-down.
pub fn window_drag_area(div: gpui::Div, double_click_zoom: bool) -> gpui::Div {
    let div = div.window_control_area(gpui::WindowControlArea::Drag);
    #[cfg(not(windows))]
    let div = div.on_mouse_down(
        MouseButton::Left,
        move |event: &MouseDownEvent, window: &mut Window, _cx: &mut gpui::App| {
            if double_click_zoom && event.click_count == 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        },
    );
    #[cfg(windows)]
    let _discarded = double_click_zoom;
    div
}
