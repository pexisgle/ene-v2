//! Application choices rendered by GPUI Kit's searchable select.

use ene_client_core::domain::ModelAssignment;
use gpui::component::{
    IndexPath,
    searchable_list::{SearchableListItem, SearchableVec},
    select::{Select, SelectEvent, SelectState},
};
use gpui::{App, Entity, SharedString, Subscription, Window, div, prelude::*, px};
use std::rc::Rc;

pub trait ChoiceValue: Clone + Eq + 'static {
    fn contains_query(&self, query: &str) -> bool;
}

impl ChoiceValue for String {
    fn contains_query(&self, query: &str) -> bool {
        self.to_lowercase().contains(query)
    }
}

impl ChoiceValue for Option<ModelAssignment> {
    fn contains_query(&self, query: &str) -> bool {
        self.as_ref().is_some_and(|assignment| {
            assignment.connection_id.contains_query(query)
                || assignment.model_id.contains_query(query)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownOption<V = String> {
    pub value: V,
    pub label: String,
    pub detail: Option<String>,
}

impl<V: ChoiceValue> SearchableListItem for DropdownOption<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.label.clone().into()
    }

    fn value(&self) -> &V {
        &self.value
    }

    fn matches(&self, query: &str) -> bool {
        let query = query.to_lowercase();
        self.value.contains_query(&query)
            || self.label.contains_query(&query)
            || self
                .detail
                .as_ref()
                .is_some_and(|detail| detail.contains_query(&query))
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

pub struct DropdownSpec<V = String> {
    pub id: String,
    pub selected_value: Option<V>,
    pub placeholder: SharedString,
    pub options: Vec<DropdownOption<V>>,
    pub enabled: bool,
    pub max_list_height: f32,
    pub search_placeholder: &'static str,
    pub empty_label: &'static str,
}

type OnSelect<V> = Rc<dyn Fn(V, &mut Window, &mut App)>;

#[derive(IntoElement)]
struct Dropdown<V: ChoiceValue> {
    spec: DropdownSpec<V>,
    on_select: OnSelect<V>,
}

struct Binding<V: ChoiceValue> {
    options: Vec<DropdownOption<V>>,
    select: Entity<SelectState<SearchableVec<DropdownOption<V>>>>,
    on_select: OnSelect<V>,
    _confirmed: Subscription,
}

impl<V: ChoiceValue> RenderOnce for Dropdown<V> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Dropdown { spec, on_select } = self;
        let binding = window.use_keyed_state(spec.id.clone(), cx, |_, _| None::<Binding<V>>);
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
            let changed = binding.options != spec.options;
            if changed {
                binding.options = spec.options;
            }
            if changed || binding.select.read(cx).selected_value() != value {
                binding.select.update(cx, |state, cx| {
                    if changed {
                        state.set_items(SearchableVec::new(binding.options.clone()), window, cx);
                    }
                    if state.selected_value() != value {
                        match value {
                            Some(value) => state.set_selected_value(value, window, cx),
                            None => state.set_selected_index(None::<IndexPath>, window, cx),
                        }
                    }
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

pub fn dropdown_select<V: ChoiceValue>(
    spec: DropdownSpec<V>,
    on_select: impl Fn(V, &mut Window, &mut App) + 'static,
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

    struct Picker<V> {
        selected: Option<V>,
        options: Vec<DropdownOption<V>>,
    }

    impl<V: ChoiceValue> Render for Picker<V> {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let picker = cx.weak_entity();
            div().w(px(320.)).child(dropdown_select(
                DropdownSpec {
                    id: "test-picker".into(),
                    selected_value: self.selected.clone(),
                    placeholder: "話者を選択".into(),
                    options: self.options.clone(),
                    enabled: true,
                    max_list_height: 148.,
                    search_placeholder: "話者を検索…",
                    empty_label: "話者が見つかりません",
                },
                move |value, _, cx| {
                    _ = picker.update(cx, |this, cx| {
                        this.selected = Some(value);
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
                cx.new(|_| Picker {
                    selected: None,
                    options: vec![DropdownOption {
                        value: "voice".to_owned(),
                        label: "日本語".into(),
                        detail: None,
                    }],
                })
            })
            .expect("open picker test window")
        });
        window
            .update(cx, |_, window, cx| {
                window.click("input", cx);
                window.press("escape", cx);
            })
            .unwrap();
        cx.read(|cx| assert!(picker.read(cx).selected.is_none()));
        window
            .update(cx, |_, window, cx| {
                window.click("input", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
        cx.read(|cx| assert_eq!(picker.read(cx).selected.as_deref(), Some("voice")));
    }

    #[gpui::test]
    fn keyboard_confirmation_delivers_a_cleared_model_assignment(cx: &mut TestAppContext) {
        let (window, picker) = cx.update(|cx| {
            crate::ui::theme::init_components(cx);
            gpui::open_window(WindowOptions::default(), cx, |_, cx| {
                cx.new(|_| Picker {
                    selected: None,
                    options: vec![DropdownOption {
                        value: None::<ModelAssignment>,
                        label: "未設定".into(),
                        detail: None,
                    }],
                })
            })
            .expect("open role choice test window")
        });
        window
            .update(cx, |_, window, cx| {
                window.click("input", cx);
                window.press("down", cx);
                window.press("enter", cx);
            })
            .unwrap();
        cx.read(|cx| assert_eq!(picker.read(cx).selected, Some(None)));
    }
}
