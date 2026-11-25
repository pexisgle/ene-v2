//! The searchable dropdown.

use crate::input::Draft;
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::Theme;
use gpui::{
    AnyElement, Bounds, Entity, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, SharedString,
    Window, anchored, deferred, div, point, prelude::*, px,
};
use std::{cell::Cell, rc::Rc};

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
/// Searchable dropdown selector built to `DESIGN.md` Strawberry Milk Capsule rules.
///
/// The menu renders through `deferred(anchored(..))`, so it floats above the
/// surrounding content instead of pushing it down.
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
    let open = spec.open && spec.enabled;
    let close: Rc<T> = Rc::new(on_toggle);
    // A press on the trigger is outside the menu, so it would close the menu
    // that the trigger is about to toggle. Ignore presses inside the trigger.
    let trigger_bounds: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let mut root = div()
        // Must precede `.id()`: only `Div` has `on_children_prepainted`.
        .on_children_prepainted({
            let trigger_bounds = Rc::clone(&trigger_bounds);
            move |children, _, _| {
                if let Some(bounds) = children.first() {
                    trigger_bounds.set(*bounds);
                }
            }
        })
        .id(SharedString::from(format!("{}-root", spec.id)))
        // Focusable so `Escape` reaches this subtree: clicking the trigger focuses
        // the root, and the open menu hangs off it.
        .focusable()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .on_key_down({
            let close = Rc::clone(&close);
            move |event: &KeyDownEvent, window, cx| {
                if open && event.keystroke.key == "escape" {
                    close(window, cx);
                }
            }
        })
        .child(dropdown_trigger(&spec, theme, &close));
    if open {
        // Zero-height slot below the trigger: `anchored()` measures from the
        // containing block, so the menu opens just under the trigger and the
        // `deferred` draw keeps it out of the surrounding layout.
        root = root.child(
            div()
                .w_full()
                .child(deferred(anchored().offset(point(px(0.), px(4.))).child(
                    dropdown_menu(&spec, theme, &close, &trigger_bounds, on_select),
                ))),
        );
    }
    root.into_any_element()
}
fn dropdown_trigger<T>(spec: &DropdownSpec<'_>, theme: &Theme, on_toggle: &Rc<T>) -> AnyElement
where
    T: Fn(&mut Window, &mut gpui::App) + 'static,
{
    let display_label = spec
        .selected_label
        .filter(|s| !s.is_empty())
        .map_or_else(|| spec.placeholder.to_owned(), str::to_owned);
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
        .rounded(crate::ui::theme::radius::medium())
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
        let on_toggle = Rc::clone(on_toggle);
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
fn dropdown_menu<T, S>(
    spec: &DropdownSpec<'_>,
    theme: &Theme,
    on_toggle: &Rc<T>,
    trigger_bounds: &Rc<Cell<Bounds<Pixels>>>,
    on_select: S,
) -> gpui::Div
where
    T: Fn(&mut Window, &mut gpui::App) + 'static,
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
        .occlude()
        .min_w(px(240.))
        .max_w(px(360.))
        .rounded(crate::ui::theme::radius::medium())
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .p(px(6.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .on_mouse_down_out({
            let on_toggle = Rc::clone(on_toggle);
            let trigger_bounds = Rc::clone(trigger_bounds);
            move |event: &MouseDownEvent, window, cx| {
                if !trigger_bounds.get().contains(&event.position) {
                    on_toggle(window, cx);
                }
            }
        })
        .when_some(search_bar, gpui::ParentElement::child)
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
        .rounded(crate::ui::theme::radius::small())
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
