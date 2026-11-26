use super::ConnectionsPane;
use crate::ui::icons::Icon;
use crate::ui::studio::labeled_field;
use crate::ui::theme::Theme;
use crate::ui::widgets::{Tone, pill_button, toggle_switch};
use ene_client_core::{
    connections::form::{AgentPlatformOptions, FieldKind, FormValue, Options, OptionsSchema},
    domain::ConnectionEndpoint,
};
use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};

pub(super) fn render_agent_platform_options(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    endpoint: Option<&ConnectionEndpoint>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (Vec<AnyElement>, Option<String>) {
    let options = endpoint
        .and_then(|ep| serde_json::from_str::<AgentPlatformOptions>(&ep.options).ok())
        .unwrap_or_default();
    let project = pane.drafts.draft(
        cx,
        format!("conn:{id}:r{rev}:project_id"),
        "Express mode では空欄",
        options.project_id.as_deref(),
        theme,
    );
    let location = pane.drafts.draft(
        cx,
        format!("conn:{id}:r{rev}:location"),
        "未指定時は global",
        options.location.as_deref(),
        theme,
    );
    let next_opts = AgentPlatformOptions::new(project.read(cx).text(), location.read(cx).text());
    (
        vec![
            labeled_field("Google Cloud プロジェクト ID", project, theme).into_any_element(),
            labeled_field("ロケーション", location, theme).into_any_element(),
        ],
        Some(next_opts.json()),
    )
}

pub(super) fn render_options_schema(
    pane: &mut ConnectionsPane,
    id: &str,
    rev: u64,
    endpoint: Option<&ConnectionEndpoint>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> (Vec<AnyElement>, Option<String>) {
    let Some(ep) = endpoint else {
        return (Vec::new(), None);
    };
    let Ok(schema) = serde_json::from_str::<OptionsSchema>(&ep.options_schema) else {
        return (Vec::new(), None);
    };
    let mut opts: Options = serde_json::from_str(&ep.options).unwrap_or_default();
    let mut controls = Vec::new();
    for (key, prop) in schema.properties {
        let prop_type = prop.kind.scalar();
        let current_val = opts.get(&key).cloned();
        let d_key = format!("conn:{id}:r{rev}:opt:{key}");

        if let Some(choices) = prop.choices() {
            let control = render_schema_enum_field(
                pane,
                SchemaFieldCtx {
                    id,
                    key: &key,
                    d_key,
                    current_val: current_val.as_ref(),
                    opts: &mut opts,
                },
                choices,
                theme,
                cx,
            );
            controls.push(control);
        } else if matches!(prop_type, FieldKind::Boolean) {
            let control = render_schema_bool_field(
                pane,
                SchemaFieldCtx {
                    id,
                    key: &key,
                    d_key,
                    current_val: current_val.as_ref(),
                    opts: &mut opts,
                },
                theme,
                cx,
            );
            controls.push(control);
        } else {
            let control = render_schema_scalar_field(
                pane,
                SchemaFieldCtx {
                    id,
                    key: &key,
                    d_key,
                    current_val: current_val.as_ref(),
                    opts: &mut opts,
                },
                prop_type,
                theme,
                cx,
            );
            controls.push(control);
        }
    }

    let serialized = (!opts.is_empty()).then(|| serde_json::json!(opts).to_string());
    (controls, serialized)
}

fn render_schema_enum_field<'a>(
    pane: &mut ConnectionsPane,
    ctx: SchemaFieldCtx<'_>,
    choices: impl Iterator<Item = &'a str>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let init = ctx
        .current_val
        .and_then(FormValue::as_text)
        .map_or_default(str::to_owned);
    let d = pane.drafts.draft(cx, ctx.d_key, "", Some(&init), theme);
    let active_val = d.read(cx).text().to_owned();
    if !active_val.is_empty() {
        ctx.opts
            .insert(ctx.key.to_owned(), FormValue::Text(active_val.clone()));
    }
    let chips: Vec<AnyElement> = choices
        .map(|s| {
            let picked = active_val == s;
            let target_d = d.clone();
            let val_str = s.to_owned();
            pill_button(
                SharedString::from(format!("opt-enum-{}-{}-{s}", ctx.id, ctx.key)),
                s.to_owned(),
                if picked { Tone::Primary } else { Tone::Ghost }
                    .maybe_icon(picked.then_some(Icon::Check)),
                theme,
                move |_, cx| {
                    target_d.update(cx, |draft, cx| draft.set(val_str.clone(), cx));
                },
            )
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child(ctx.key.to_owned()),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(6.))
                .children(chips),
        )
        .into_any_element()
}

fn render_schema_bool_field(
    pane: &mut ConnectionsPane,
    ctx: SchemaFieldCtx<'_>,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let init = ctx
        .current_val
        .and_then(FormValue::as_bool)
        .map_or("false", |b| if b { "true" } else { "false" });
    let d = pane.drafts.draft(cx, ctx.d_key, "", Some(init), theme);
    let checked = d.read(cx).text() == "true";
    ctx.opts
        .insert(ctx.key.to_owned(), FormValue::Boolean(checked));
    let target_d = d.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child(ctx.key.to_owned()),
        )
        .child(toggle_switch(
            SharedString::from(format!("opt-bool-{}-{}", ctx.id, ctx.key)),
            checked,
            true,
            theme,
            move |_, cx| {
                target_d.update(cx, |draft, cx| {
                    draft.set(if checked { "false" } else { "true" }, cx);
                });
            },
        ))
        .into_any_element()
}

fn render_schema_scalar_field(
    pane: &mut ConnectionsPane,
    ctx: SchemaFieldCtx<'_>,
    prop_type: FieldKind,
    theme: &Theme,
    cx: &mut Context<ConnectionsPane>,
) -> AnyElement {
    let init = ctx.current_val.map(FormValue::draft_text);
    let d = pane.drafts.draft(cx, ctx.d_key, "", init.as_deref(), theme);
    let txt = d.read(cx).text().trim().to_owned();
    if !txt.is_empty() {
        ctx.opts
            .insert(ctx.key.to_owned(), FormValue::from_draft(prop_type, &txt));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child(ctx.key.to_owned()),
        )
        .child(d)
        .into_any_element()
}

struct SchemaFieldCtx<'a> {
    id: &'a str,
    key: &'a str,
    d_key: String,
    current_val: Option<&'a FormValue>,
    opts: &'a mut Options,
}
