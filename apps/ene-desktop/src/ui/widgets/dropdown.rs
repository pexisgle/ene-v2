//! Search and selection for app data shown in a GPUI Kit popover.

use crate::input::Draft;
use crate::ui::icons::{Icon, icon};
use crate::ui::theme::{Theme, radius};
use gpui::component::Disableable;
use gpui::component::button::{Button, ButtonVariants};
use gpui::component::list::ListItem;
use gpui::component::popover::Popover;
use gpui::{AnyElement, App, Entity, SharedString, StyleRefinement, Window, div, prelude::*, px};
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownOption {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

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

pub fn dropdown_select<T, S>(
    spec: DropdownSpec<'_>,
    theme: &Theme,
    on_toggle: T,
    on_select: S,
) -> AnyElement
where
    T: Fn(&mut Window, &mut App) + 'static,
    S: Fn(String, &mut Window, &mut App) + 'static,
{
    let selected_label = spec.selected_label.filter(|label| !label.is_empty());
    let fg = if spec.enabled && selected_label.is_some() {
        theme.ink
    } else {
        theme.ink_placeholder
    };
    let trigger = Button::new(SharedString::from(format!("{}-trigger", spec.id)))
        .ghost()
        .disabled(!spec.enabled)
        .w_full()
        .min_w_0()
        .justify_between()
        .rounded(radius::medium())
        .bg(if spec.enabled {
            theme.bg_input
        } else {
            theme.bg_blush
        })
        .border_1()
        .border_color(if spec.open {
            theme.accent
        } else {
            theme.border
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(fg)
                .truncate()
                .child(selected_label.unwrap_or(spec.placeholder).to_owned()),
        )
        .child(icon(
            if spec.open {
                Icon::ChevronUp
            } else {
                Icon::ChevronDown
            },
            14.,
            theme.ink_muted,
        ));
    if !spec.enabled {
        return trigger.into_any_element();
    }

    let open = spec.open;
    let menu = open.then(|| dropdown_menu(&spec, theme, on_select));
    Popover::new(SharedString::from(spec.id))
        .trigger(trigger)
        .trigger_style(StyleRefinement::default().w_full().min_w_0())
        .open(open)
        .on_open_change(move |next, window, cx| {
            if *next != open {
                on_toggle(window, cx);
            }
        })
        .p(px(6.))
        .children(menu)
        .into_any_element()
}

fn dropdown_menu(
    spec: &DropdownSpec<'_>,
    theme: &Theme,
    on_select: impl Fn(String, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let query = spec.filter_text.trim().to_lowercase();
    let on_select = Rc::new(on_select);
    let rows: Vec<_> = spec
        .options
        .iter()
        .filter(|option| option_matches_query(option, &query))
        .enumerate()
        .map(|(idx, option)| {
            let selected = spec.selected_value == Some(option.value.as_str());
            let select = on_select.clone();
            let value = option.value.clone();
            let label = SharedString::from(option.label.clone());
            ListItem::new(idx)
                .selected(selected)
                .accessibility_label(label.clone())
                .on_click(move |_, window, cx| select(value.clone(), window, cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_xs().truncate().child(label))
                        .when_some(option.detail.clone(), |this, detail| {
                            this.child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.ink_muted)
                                    .truncate()
                                    .child(detail),
                            )
                        }),
                )
        })
        .collect();
    let empty = rows.is_empty();
    div()
        .min_w(px(240.))
        .max_w(px(360.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .when_some(
            spec.filter_draft.clone().filter(|_| spec.options.len() > 8),
            gpui::ParentElement::child,
        )
        .child(
            div()
                .id("options")
                .max_h(px(spec.max_list_height))
                .overflow_y_scroll()
                .children(rows)
                .when(empty, |this| {
                    let message = if query.is_empty() {
                        spec.empty_label
                    } else {
                        "該当する候補がありません"
                    };
                    this.child(
                        div()
                            .p_2()
                            .text_xs()
                            .text_color(theme.ink_muted)
                            .child(message.to_owned()),
                    )
                }),
        )
        .into_any_element()
}

fn option_matches_query(option: &DropdownOption, query: &str) -> bool {
    query.is_empty()
        || option.label.to_lowercase().contains(query)
        || option.value.to_lowercase().contains(query)
        || option
            .detail
            .as_deref()
            .is_some_and(|detail| detail.to_lowercase().contains(query))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::test::TestWindowExt;
    use gpui::{AppContext, Context, TestAppContext, WindowOptions};

    #[derive(Default)]
    struct Picker {
        open: bool,
        selected: Option<String>,
    }

    impl Render for Picker {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let entity = cx.entity();
            let select = entity.clone();
            let options = [DropdownOption {
                value: "voice".into(),
                label: "話者".into(),
                detail: Some("日本語".into()),
            }];
            div().w(px(320.)).child(dropdown_select(
                DropdownSpec {
                    id: "voice-picker".into(),
                    selected_value: self.selected.as_deref(),
                    selected_label: self.selected.as_ref().map(|_| "話者"),
                    placeholder: "話者を選択",
                    options: &options,
                    open: self.open,
                    enabled: true,
                    max_list_height: 148.,
                    filter_draft: None,
                    filter_text: "",
                    empty_label: "話者が見つかりません",
                },
                &Theme::default(),
                move |_, cx| {
                    entity.update(cx, |picker, cx| {
                        picker.open = !picker.open;
                        cx.notify();
                    });
                },
                move |value, _, cx| {
                    select.update(cx, |picker, cx| {
                        picker.selected = Some(value);
                        picker.open = false;
                        cx.notify();
                    });
                },
            ))
        }
    }

    #[gpui::test]
    fn dropdown_selects_a_row_and_dismisses_with_escape(cx: &mut TestAppContext) {
        let (window, picker) = cx.update(|cx| {
            crate::ui::theme::init_components(cx);
            gpui::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| Picker::default())
            })
            .expect("open picker test window")
        });
        window
            .update(cx, |_, window, cx| {
                window.click("voice-picker-trigger", cx);
                assert!(picker.read(cx).open);
                window.click(0usize, cx);
                assert_eq!(picker.read(cx).selected.as_deref(), Some("voice"));
                assert!(!picker.read(cx).open);

                window.click("voice-picker-trigger", cx);
                assert!(picker.read(cx).open);
                window.press("escape", cx);
                assert!(!picker.read(cx).open);
            })
            .unwrap();
    }
}
