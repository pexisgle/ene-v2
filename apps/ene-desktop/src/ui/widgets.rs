//! Reusable GPUI UI primitives: capsule pills, detached orbs, toggle switches,
//! companion emblems and progressive disclosure controls.

use super::icons::{Icon, icon};
use super::theme::{Theme, companion_palette};
use crate::input::Draft;
use gpui::{
    AnyElement, ElementId, Entity, Hsla, MouseButton, MouseDownEvent, SharedString, Window, div,
    prelude::*, px,
};
use std::rc::Rc;

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
    let _ = double_click_zoom;
    div
}

/// Option item shown in [`dropdown_select`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownOption {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

/// Configuration for [`dropdown_select`].
pub struct DropdownSpec<'a> {
    pub id: String,
    pub selected_value: Option<&'a str>,
    pub selected_label: Option<&'a str>,
    pub placeholder: &'a str,
    pub options: &'a [DropdownOption],
    pub open: bool,
    pub enabled: bool,
    pub max_list_height: f32,
    pub filter_draft: Option<Entity<Draft>>,
    pub filter_text: &'a str,
    pub empty_label: &'a str,
}

/// Searchable inline dropdown selector built to `DESIGN.md` Strawberry Milk Capsule rules.
pub fn dropdown_select<T, S>(
    spec: DropdownSpec<'_>,
    theme: &Theme,
    on_toggle: T,
    on_select: S,
) -> AnyElement
where
    T: Fn(&mut Window, &mut gpui::App) + 'static,
    S: Fn(String, &mut Window, &mut gpui::App) + 'static,
{
    let trigger = dropdown_trigger(&spec, theme, on_toggle);
    let mut root = div().w_full().min_w_0().flex().flex_col().child(trigger);
    if spec.open && spec.enabled {
        root = root.child(dropdown_menu(&spec, theme, on_select));
    }
    root.into_any_element()
}

fn dropdown_trigger<T>(spec: &DropdownSpec<'_>, theme: &Theme, on_toggle: T) -> AnyElement
where
    T: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let display_label = spec
        .selected_label
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| spec.placeholder.to_owned());
    let has_selection = spec.selected_label.is_some_and(|s| !s.is_empty());
    let trigger_bg = if spec.enabled {
        theme.bg_input
    } else {
        theme.bg_blush
    };
    let trigger_border = if spec.open && spec.enabled {
        theme.accent
    } else {
        theme.border
    };
    let trigger_fg = if spec.enabled && has_selection {
        theme.ink
    } else {
        theme.ink_placeholder
    };
    let hover_border = theme.border_float;
    let trigger = div()
        .id(SharedString::from(format!("{}-trigger", spec.id)))
        .w_full()
        .min_w_0()
        .px(px(12.))
        .py(px(7.))
        .rounded(px(10.))
        .bg(trigger_bg)
        .border_2()
        .border_color(trigger_border)
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(trigger_fg)
                .truncate()
                .child(display_label),
        )
        .child(icon(
            if spec.open && spec.enabled {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            14.,
            if spec.enabled {
                theme.ink_muted
            } else {
                theme.ink_placeholder
            },
        ));
    if spec.enabled {
        trigger
            .cursor_pointer()
            .hover(move |this| this.border_color(hover_border))
            .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                on_toggle(window, cx);
            })
            .into_any_element()
    } else {
        trigger.into_any_element()
    }
}

fn dropdown_menu<S>(spec: &DropdownSpec<'_>, theme: &Theme, on_select: S) -> gpui::Div
where
    S: Fn(String, &mut Window, &mut gpui::App) + 'static,
{
    let on_select = Rc::new(on_select);
    let query = spec.filter_text.trim().to_lowercase();
    let show_search = spec.options.len() > 8 && spec.filter_draft.is_some();
    let mut rows: Vec<AnyElement> = spec
        .options
        .iter()
        .filter(|opt| option_matches_query(opt, &query))
        .enumerate()
        .map(|(idx, opt)| {
            let selected = spec.selected_value == Some(opt.value.as_str());
            dropdown_option_row(&spec.id, idx, opt, selected, theme, &on_select)
        })
        .collect();

    if rows.is_empty() {
        let msg = if query.is_empty() {
            spec.empty_label
        } else {
            "該当する候補がありません"
        };
        rows.push(
            div()
                .px(px(10.))
                .py(px(8.))
                .text_xs()
                .text_color(theme.ink_muted)
                .child(msg.to_owned())
                .into_any_element(),
        );
    }

    let search_bar = spec
        .filter_draft
        .clone()
        .filter(|_| show_search)
        .map(|draft| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.))
                .child(icon(Icon::Search, 13., theme.ink_muted))
                .child(div().flex_1().min_w_0().child(draft))
        });

    div()
        .mt(px(4.))
        .w_full()
        .rounded(px(10.))
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .p(px(6.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .when_some(search_bar, |this, bar| this.child(bar))
        .child(
            div()
                .id(SharedString::from(format!("{}-scroll", spec.id)))
                .w_full()
                .max_h(px(spec.max_list_height))
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap(px(2.))
                .children(rows),
        )
}

fn option_matches_query(opt: &DropdownOption, query: &str) -> bool {
    query.is_empty()
        || opt.label.to_lowercase().contains(query)
        || opt.value.to_lowercase().contains(query)
        || opt
            .detail
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains(query))
}

fn dropdown_option_row<S>(
    id_base: &str,
    idx: usize,
    opt: &DropdownOption,
    is_selected: bool,
    theme: &Theme,
    on_select: &Rc<S>,
) -> AnyElement
where
    S: Fn(String, &mut Window, &mut gpui::App) + 'static,
{
    let val = opt.value.clone();
    let select_cb = Rc::clone(on_select);
    let hover_bg = theme.bg_blush;
    let row_bg = if is_selected {
        theme.accent_soft
    } else {
        theme.bg_surface
    };
    div()
        .id(SharedString::from(format!("{id_base}-opt-{idx}")))
        .w_full()
        .min_w_0()
        .px(px(10.))
        .py(px(6.))
        .rounded(px(8.))
        .bg(row_bg)
        .cursor_pointer()
        .hover(move |this| this.bg(hover_bg))
        .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
            select_cb(val.clone(), window, cx);
        })
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
                .gap(px(1.))
                .child(
                    div()
                        .text_xs()
                        .font_weight(if is_selected {
                            gpui::FontWeight::BOLD
                        } else {
                            gpui::FontWeight::MEDIUM
                        })
                        .text_color(if is_selected {
                            theme.accent_soft_fg
                        } else {
                            theme.ink
                        })
                        .truncate()
                        .child(opt.label.clone()),
                )
                .when_some(opt.detail.clone(), |this, detail| {
                    this.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.ink_muted)
                            .truncate()
                            .child(detail),
                    )
                }),
        )
        .when(is_selected, |this| {
            this.child(icon(Icon::Check, 13., theme.accent))
        })
        .into_any_element()
}
