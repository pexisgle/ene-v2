//! Endpoint-option controls for the connection editor.

use super::ConnectionsPane;
use crate::ui::{studio::labeled_field, theme::Theme};
use ene_client_core::{
    connections::form::{AgentPlatformOptions, SchemaOptions},
    domain::ConnectionEndpoint,
};
use ene_domain::DomainError;
use gpui::{AnyElement, Context, prelude::*};

type Controls = (Vec<AnyElement>, Result<Option<String>, DomainError>);

pub(super) fn render_agent_platform_options(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    endpoint: Option<&ConnectionEndpoint>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Controls {
    let options = match AgentPlatformOptions::load(endpoint.map_or("", |ep| ep.options.as_str())) {
        Ok(options) => options,
        Err(error) => return (Vec::new(), Err(error)),
    };
    let project = pane.drafts.draft(
        cx,
        format!("conn:{id}:r{rev}:project_id"),
        "Express mode では空欄",
        options.text("project_id"),
        theme,
    );
    let location = pane.drafts.draft(
        cx,
        format!("conn:{id}:r{rev}:location"),
        "未指定時は global",
        options.text("location"),
        theme,
    );
    let updated = options.apply(project.read(cx).text(), location.read(cx).text());
    (
        vec![
            labeled_field("Google Cloud プロジェクト ID", project, theme).into_any_element(),
            labeled_field("ロケーション", location, theme).into_any_element(),
        ],
        Ok(updated),
    )
}

pub(super) fn render_options_schema(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    endpoint: Option<&ConnectionEndpoint>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> Controls {
    let Some(endpoint) = endpoint else {
        return (Vec::new(), Ok(None));
    };
    let options = match SchemaOptions::from_endpoint(endpoint) {
        Ok(Some(options)) => options,
        Ok(None) => return (Vec::new(), Ok(None)),
        Err(error) => return (Vec::new(), Err(error)),
    };
    let visible = options.visible();
    let initial = serde_json::to_string_pretty(&visible).unwrap_or_else(|_| "{}".into());
    let draft = pane.drafts.description_draft(
        cx,
        format!("conn:{id}:r{rev}:options"),
        "{}",
        Some(&initial),
        theme,
    );
    let updated = options.apply(&visible, draft.read(cx).text());
    (
        vec![labeled_field("詳細オプション（JSON）", draft, theme).into_any_element()],
        updated,
    )
}
