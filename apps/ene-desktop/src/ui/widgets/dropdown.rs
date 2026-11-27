//! Application choices rendered by GPUI Kit's searchable select.

use gpui::component::{
    IndexPath,
    searchable_list::{SearchableListItem, SearchableVec},
    select::{Select, SelectEvent, SelectState},
};
use gpui::{App, Entity, SharedString, Subscription, Window, div, prelude::*, px};
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownOption {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

impl SearchableListItem for DropdownOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &String {
        &self.value
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        [&self.label, &self.value]
            .into_iter()
            .chain(self.detail.iter())
            .any(|text| text.to_lowercase().contains(&query))
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(div().truncate().child(self.title()))
            .when_some(self.detail.clone(), |row, detail| {
                row.child(div().truncate().text_xs().opacity(0.7).child(detail))
            })
    }
}

pub struct DropdownSpec {
    pub id: String,
    pub selected_value: Option<String>,
    pub placeholder: SharedString,
    pub options: Vec<DropdownOption>,
    pub enabled: bool,
    pub max_list_height: f32,
    pub search_placeholder: &'static str,
    pub empty_label: &'static str,
}

type OnSelect = Rc<dyn Fn(String, &mut Window, &mut App)>;

#[derive(IntoElement)]
struct Dropdown {
    spec: DropdownSpec,
    on_select: OnSelect,
}

struct Binding {
    options: Vec<DropdownOption>,
    select: Entity<SelectState<SearchableVec<DropdownOption>>>,
    on_select: OnSelect,
    _confirmed: Subscription,
}

impl RenderOnce for Dropdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Dropdown { spec, on_select } = self;
        let binding = window.use_keyed_state(spec.id.clone(), cx, |_, _| None::<Binding>);
        let select = binding.update(cx, |state, cx| {
            let selected_index = spec.selected_value.as_ref().and_then(|value| {
                spec.options
                    .iter()
                    .position(|item| &item.value == value)
                    .map(|row| IndexPath::default().row(row))
            });
            let value = selected_index.and(spec.selected_value.as_ref());
            let binding = state.get_or_insert_with(|| {
                let select = cx.new(|cx| {
                    SelectState::new(
                        SearchableVec::new(spec.options.clone()),
                        selected_index,
                        window,
                        cx,
                    )
                    .searchable(true)
                });
                let _confirmed =
                    cx.subscribe_in(&select, window, |binding, _, event, window, cx| {
                        if let (Some(binding), SelectEvent::Confirm(Some(value))) = (binding, event)
                        {
                            (binding.on_select)(value.clone(), window, cx);
                        }
                    });
                Binding {
                    options: spec.options.clone(),
                    select,
                    on_select: on_select.clone(),
                    _confirmed,
                }
            });
            binding.on_select = on_select;
            if binding.options != spec.options {
                binding.options = spec.options;
                binding.select.update(cx, |state, cx| {
                    state.set_items(SearchableVec::new(binding.options.clone()), window, cx);
                    // Set the committed value on the full list after refreshing the items.
                    select_value(state, value, window, cx);
                });
            } else if binding.select.read(cx).selected_value() != value {
                binding.select.update(cx, |state, cx| {
                    select_value(state, value, window, cx);
                });
            }
            binding.select.clone()
        });
        Select::new(&select)
            .id(spec.id)
            .w_full()
            .min_w_0()
            .disabled(!spec.enabled)
            .menu_max_h(px(spec.max_list_height))
            .placeholder(spec.placeholder)
            .search_placeholder(spec.search_placeholder)
            .empty(move |_, _| div().p_2().text_xs().child(spec.empty_label))
    }
}

fn select_value(
    state: &mut SelectState<SearchableVec<DropdownOption>>,
    value: Option<&String>,
    window: &mut Window,
    cx: &mut gpui::Context<SelectState<SearchableVec<DropdownOption>>>,
) {
    if let Some(value) = value {
        state.set_selected_value(value, window, cx);
    } else if state.selected_value().is_some() {
        state.set_selected_index(None::<IndexPath>, window, cx);
    }
}

pub fn dropdown_select(
    spec: DropdownSpec,
    on_select: impl Fn(String, &mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    Dropdown {
        spec,
        on_select: Rc::new(on_select),
    }
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::test::TestWindowExt;
    use gpui::{AppContext, Context, TestAppContext, WindowOptions};

    #[derive(Default)]
    struct Picker(Option<String>);

    impl Render for Picker {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let picker = cx.weak_entity();
            div().w(px(320.)).child(dropdown_select(
                DropdownSpec {
                    id: "test-picker".into(),
                    selected_value: self.0.clone(),
                    placeholder: "話者を選択".into(),
                    options: vec![DropdownOption {
                        value: "voice".into(),
                        label: "日本語".into(),
                        detail: None,
                    }],
                    enabled: true,
                    max_list_height: 148.,
                    search_placeholder: "話者を検索…",
                    empty_label: "話者が見つかりません",
                },
                move |value, _, cx| {
                    _ = picker.update(cx, |this, cx| {
                        this.0 = Some(value);
                        cx.notify();
                    });
                },
            ))
        }
    }

    #[gpui::test]
    fn keyboard_confirmation_commits_and_escape_dismisses(cx: &mut TestAppContext) {
        let (window, picker) = cx.update(|cx| {
            crate::ui::theme::init_components(cx);
            gpui::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| Picker::default())
            })
            .expect("open picker test window")
        });
        window
            .update(cx, |_, window, cx| {
                window.click("input", cx);
                window.press("escape", cx);
            })
            .unwrap();
        cx.read(|cx| assert!(picker.read(cx).0.is_none()));
        window
            .update(cx, |_, window, cx| {
                window.click("input", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
        cx.read(|cx| assert_eq!(picker.read(cx).0.as_deref(), Some("voice")));
    }
}
