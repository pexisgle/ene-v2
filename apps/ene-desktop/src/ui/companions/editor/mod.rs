//! The inline companion editor.
//!
//! Per-companion profile, VRM model, voice binding and mind profile.

mod form;
mod views;
mod voice_view;

pub(crate) use form::EditorContext;
use form::*;
use views::*;
use voice_view::*;

use super::{CompanionsPane, parse_voice_selection};
use crate::ui::{CompanionProfilePatch, studio::labeled_field, theme::Theme};
use ene_client_core::domain::{BodyBinding, VoiceBinding};
use gpui::{AnyElement, Context, div, prelude::*, px};

pub(super) fn render_companion_editor(
    pane: &mut CompanionsPane,
    ctx: EditorContext<'_>,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let id = ctx.comp.id.clone();
    let current_body = ctx
        .comp
        .body
        .as_ref()
        .and_then(|doc| serde_json::from_str::<BodyBinding>(&doc.json).ok())
        .unwrap_or_default();
    let drafts = init_companion_editor_drafts(pane, ctx.comp, &current_body, theme, cx);

    let name_val = drafts.name_d.read(cx).text().trim().to_owned();
    let mind_val = drafts.mind_d.read(cx).text().trim().to_owned();
    let persona_val = drafts.persona_d.read(cx).text().trim().to_owned();
    let body_model_val = drafts.body_model_d.read(cx).text().trim().to_owned();
    let voice_key_val = drafts.voice_key_d.read(cx).text().to_owned();
    let voice_manual_val = drafts.voice_d.read(cx).text().trim().to_owned();
    let (picked_conn_id, picked_voice_id) =
        parse_voice_selection(&voice_key_val, &voice_manual_val);
    let mut next_body = current_body;
    next_body.model = (!body_model_val.is_empty()).then_some(body_model_val);
    let next_voice = VoiceBinding {
        connection_id: picked_conn_id.clone(),
        voice_id: picked_voice_id.clone(),
        speed: parse_finite_f32(&drafts.voice_speed_d, cx),
        pitch: parse_finite_f32(&drafts.voice_pitch_d, cx),
        style: non_empty_trimmed(&drafts.voice_style_d, cx),
    };

    let model_field = render_body_model_field(&id, drafts.body_model_d.clone(), theme);
    let voice_selector = render_voice_selector(
        pane,
        &ctx,
        &drafts,
        (
            &voice_key_val,
            picked_conn_id.as_deref(),
            picked_voice_id.as_deref(),
        ),
        theme,
        cx,
    );
    let (disclosure, mind_strip) = render_mind_section(pane, &ctx, &mind_val, theme, cx);
    let footer = render_companion_footer(
        pane,
        &id,
        ctx.live,
        CompanionProfilePatch {
            companion_id: id.clone(),
            name: name_val,
            persona: persona_val,
            mind: mind_val,
            body: next_body,
            voice: next_voice,
        },
        theme,
        cx,
    );

    div()
        .border_t_1()
        .border_color(theme.border)
        .px(px(16.))
        .py(px(16.))
        .flex()
        .flex_col()
        .gap(px(12.))
        .child(labeled_field("名前", drafts.name_d, theme))
        .child(labeled_field("人格の核", drafts.mind_d, theme))
        .child(labeled_field("話し方", drafts.persona_d, theme))
        .child(model_field)
        .child(voice_selector)
        .child(disclosure)
        .when_some(mind_strip, |this, strip| this.child(strip))
        .when_some(ctx.exported_json, |this, json| {
            this.child(render_exported_template_box(&id, json, theme))
        })
        .child(footer)
        .into_any_element()
}
