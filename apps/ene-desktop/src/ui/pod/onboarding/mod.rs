//! First-run onboarding: connection, model and voice setup.
//!
//! The body picks one of two branches; each branch lives in its own
//! module so neither the setup form nor the provider chrome crowds the
//! other out of the file-length budget.

mod setup;
mod views;

use super::CompanionPod;
use crate::ui::GOOGLE_AGENT_PLATFORM;
use crate::ui::theme::Theme;
use crate::ui::widgets::DropdownOption;
use gpui::{AnyElement, Context, div, prelude::*, px};

use setup::*;
use views::*;

pub(super) struct SetupChoices {
    target_conn_id: Option<String>,
    model_options: Vec<DropdownOption>,
    selected_model: Option<String>,
    selected_model_label: Option<String>,
    checking_models: bool,
    voicevox_conn_id: Option<String>,
    voicevox_enabled: bool,
    voicevox_checking: bool,
    voice_options: Vec<DropdownOption>,
    selected_voice: Option<String>,
    selected_voice_label: Option<String>,
}

pub(super) fn render_onboarding_body(
    pod: &CompanionPod,
    needs_connection: bool,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    if needs_connection {
        return render_connection_onboarding(pod, theme, cx);
    }
    render_model_voice_onboarding(pod, theme, cx)
}

pub(super) fn render_connection_onboarding(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let providers = pod
        .hub
        .read(cx)
        .model
        .connections
        .providers
        .clone()
        .unwrap_or_default();
    if providers.is_empty() {
        return render_providers_loading(&pod.hub, theme);
    }
    let picked = pod
        .selected_provider_id
        .as_deref()
        .and_then(|id| providers.iter().find(|p| p.id == id))
        .or_else(|| providers.iter().find(|p| p.id == "openrouter"))
        .or_else(|| providers.first())
        .cloned();
    let provider_chips = build_provider_chips(&providers, picked.as_ref(), theme, cx);

    let look = theme.field_look(false);
    pod.onboard_key.update(cx, |d, cx| d.apply_look(look, cx));
    let secret = pod.onboard_key.read(cx).text().trim().to_owned();
    let needs_key = picked.as_ref().is_some_and(|p| p.requires_credentials);
    let can_connect = picked.is_some() && (!needs_key || !secret.is_empty());

    let hub = pod.hub.read(cx);
    let pending = hub.setup.pending;
    let err = hub.setup.error;
    let err_detail = hub.setup.error_detail.clone();
    let name = picked
        .as_ref()
        .map_or_else(|| "AIサービス".to_owned(), |p| p.name.clone());
    let is_agent_platform = picked
        .as_ref()
        .is_some_and(|p| p.adapter == GOOGLE_AGENT_PLATFORM);
    let keys_url = picked.as_ref().and_then(|p| provider_keys_url(&p.id));
    let subtitle = onboarding_subtitle(is_agent_platform, needs_key, &name);

    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .child(render_connection_header(&pod.hub, !pending, theme))
        .child(div().text_sm().child(subtitle))
        .child(render_provider_chip_row(
            provider_chips,
            &pod.hub,
            !pending,
            theme,
        ))
        .when_some(keys_url, |this, url| {
            this.child(render_keys_link_row(url, &name, !pending, theme))
        })
        .child(pod.onboard_key.clone())
        .when_some(err, |row, msg| {
            row.child(div().text_xs().text_color(theme.danger_fg).child(msg))
        })
        .when_some(err_detail, |row, detail| {
            row.child(div().text_xs().text_color(theme.danger_fg).child(detail))
        })
        .child(render_connect_submit_row(
            ConnectSubmitSpec {
                picked,
                name: &name,
                secret,
                can_connect,
                pending,
            },
            &pod.hub,
            theme,
            cx,
        ))
        .into_any_element()
}

pub(super) fn render_model_voice_onboarding(
    pod: &CompanionPod,
    theme: &Theme,
    cx: &mut Context<CompanionPod>,
) -> AnyElement {
    let look = theme.field_look(false);
    pod.onboard_name.update(cx, |d, cx| d.apply_look(look, cx));
    pod.setup_model_filter
        .update(cx, |d, cx| d.apply_look(look, cx));
    pod.setup_voice_filter
        .update(cx, |d, cx| d.apply_look(look, cx));
    let companions_empty = pod
        .hub
        .read(cx)
        .model
        .snapshot()
        .is_none_or(|snap| snap.companions.is_empty());
    let choices = collect_setup_choices(pod, cx);

    let name = pod.onboard_name.read(cx).text().trim().to_owned();
    let can_finish = (!companions_empty || !name.is_empty())
        && ((choices.target_conn_id.is_some() && choices.selected_model.is_some())
            || (companions_empty && choices.selected_model.is_none() && !choices.checking_models));

    let mut body = div().flex().flex_col().gap(px(8.)).child(
        div()
            .font_weight(gpui::FontWeight::BOLD)
            .child("使うモデルと音声を選択"),
    );

    if let Some(refresh_cid) = choices.target_conn_id.clone() {
        body = append_setup_model_section(pod, body, &refresh_cid, &choices, theme, cx);
    }
    body = append_setup_voice_section(pod, body, &choices, theme, cx);
    if companions_empty {
        body = body
            .child(
                div()
                    .text_xs()
                    .text_color(theme.ink_muted)
                    .child("コンパニオンの名前"),
            )
            .child(pod.onboard_name.clone());
    }

    let finish_ctx = SetupFinishParams {
        companions_empty,
        name,
        conn_id: choices.target_conn_id,
        model_id: choices.selected_model,
        voice_id: choices.selected_voice,
        can_finish,
    };
    body.child(render_setup_finish_button(finish_ctx, &pod.hub, theme))
        .into_any_element()
}
