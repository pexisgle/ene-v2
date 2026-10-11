//! Search stays local to a selector and does not mutate the selected value.
use crate::{AppWindow, SelectorSearch};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::rc::Rc;

pub fn bind(ui: &AppWindow) {
    ui.global::<SelectorSearch>()
        .on_filter_options(filter_options);
    ui.global::<SelectorSearch>()
        .on_find_option(|options, value| {
            options
                .iter()
                .position(|option| option == value)
                .map_or(-1, |index| index as i32)
        });
    ui.global::<SelectorSearch>().on_is_search_text(|text| {
        text.chars()
            .any(|ch| !ch.is_control() && !('\u{e000}'..='\u{f8ff}').contains(&ch))
    });
    ui.global::<SelectorSearch>()
        .on_byte_count(|text| text.len() as i32);
}

fn filter_options(options: ModelRc<SharedString>, query: SharedString) -> ModelRc<SharedString> {
    let query = query.to_lowercase();
    let terms: Vec<_> = query.split_whitespace().collect();
    if terms.is_empty() {
        return options;
    }
    Rc::new(VecModel::from(
        options
            .iter()
            .filter(|option| {
                let name = option.to_lowercase();
                terms.iter().all(|term| name.contains(term))
            })
            .collect::<Vec<_>>(),
    ))
    .into()
}
