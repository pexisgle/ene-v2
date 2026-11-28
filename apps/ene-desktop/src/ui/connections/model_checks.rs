//! The two explicit requests against a saved dialogue connection.

use crate::ui::{
    Hub,
    theme::Theme,
    widgets::{Tone, pill_button},
};
use ene_domain::settings::MODEL_CHECK_PROMPT;
use gpui::component::StyledExt as _;
use gpui::{App, Div, Entity, SharedString, div, prelude::*, px};

pub(crate) fn model_checks(
    hub: &Entity<Hub>,
    connection_id: &str,
    selected: Option<&str>,
    theme: &Theme,
    cx: &App,
) -> Option<Div> {
    let state = hub.read(cx);
    let settings = state.model.settings()?;
    let connection = settings
        .connections
        .iter()
        .find(|row| row.id == connection_id)?;
    let receiver = state.model.dialogue_receiver(connection)?;
    let available = state.live()
        && connection.enabled
        && !state.setup.checking_connections.contains_key(connection_id);
    let revision = settings.revision;
    let test = connection
        .llm_endpoint()
        .filter(|endpoint| endpoint.adapter == crate::ui::GOOGLE_AGENT_PLATFORM)
        .map_or(
            "利用可能なモデル",
            |_| "固定文「Hi」のトークン数で利用可能なモデル",
        );
    let buttons = [("一覧を取得".to_owned(), None)]
        .into_iter()
        .chain(selected.map(|model| ("選択したモデルを確認".to_owned(), Some(model.to_owned()))))
        .map(|(label, model)| {
            let key = if model.is_some() {
                "check-model"
            } else {
                "list-models"
            };
            let id = connection_id.to_owned();
            let hub = hub.clone();
            pill_button(
                SharedString::from(format!("{key}-{id}")),
                label,
                Tone::Ghost.enabled(available),
                theme,
                move |_, cx| {
                    hub.update(cx, |hub, cx| {
                        hub.check_connection_at(id.clone(), revision, model.clone(), cx)
                    });
                },
            )
        });
    let mut disclosure =
        format!("保存済みの接続先 {receiver} に、認証情報とともに{test}を問い合わせます。");
    if let Some(model) = selected {
        disclosure.push_str(&format!(
            " {model} の確認ではテスト文「{MODEL_CHECK_PROMPT}」を送信します。"
        ));
    }
    Some(
        div()
            .v_flex()
            .gap(px(4.))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child(disclosure),
            )
            .child(div().flex().flex_wrap().gap(px(8.)).children(buttons)),
    )
}
