//! The voice selector and its header row.
//!
//! Split out of `views.rs` because they are self-contained: they read a
//! draft and a snapshot and draw, with no knowledge of the rest of the
//! editor.

//! The companion editor's visual sections.

//! Per-companion profile, VRM model, voice binding, and mind profile editor.

use super::super::CompanionsPane;
use crate::ui::{
    icons::Icon,
    studio::labeled_field,
    theme::Theme,
    widgets::{ChoiceValue, DropdownOption, DropdownSpec, Tone, dropdown_select, pill_button},
};
use ene_domain::{CompanionId, ConnectionId, VoiceBinding};
use gpui::component::StyledExt as _;
use gpui::{AnyElement, Context, Entity, FontWeight, SharedString, div, prelude::*, px};

use super::form::*;

#[derive(Clone, Default, PartialEq, Eq)]
struct VoiceChoice {
    connection: Option<ConnectionId>,
    voice: Option<String>,
}

impl ChoiceValue for VoiceChoice {
    fn contains_query(&self, query: &str) -> bool {
        self.connection
            .as_ref()
            .is_some_and(|id| id.as_str().to_lowercase().contains(query))
            || self
                .voice
                .as_ref()
                .is_some_and(|id| id.contains_query(query))
    }
}

fn voice_options(voices: &[(String, String, String, String)]) -> Vec<DropdownOption<VoiceChoice>> {
    std::iter::once(DropdownOption {
        value: VoiceChoice::default(),
        label: "接続の既定を使用".into(),
        detail: None,
    })
    .chain(
        voices
            .iter()
            .filter_map(|(connection, voice, label, name)| {
                Some(DropdownOption {
                    value: VoiceChoice {
                        connection: Some(ConnectionId::parse(connection.as_str()).ok()?),
                        voice: Some(voice.clone()),
                    },
                    label: label.clone(),
                    detail: Some(format!("{name} / ID: {voice}")),
                })
            }),
    )
    .collect()
}

pub(super) fn render_voice_selector(
    pane: &CompanionsPane,
    ctx: &EditorContext<'_>,
    drafts: &CompanionEditorDrafts,
    voice: &VoiceBinding,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let id = &ctx.comp.id;
    let selected_voice_label = format_selected_voice_label(
        ctx.tts_voices,
        voice.connection_id.as_deref(),
        voice.voice_id.as_deref(),
    );
    let pane_voice_pick = cx.weak_entity();
    let voice_d_pick = drafts.voice_d.clone();
    let draft_companion = CompanionId::parse(id.as_str()).ok();
    let revision = ctx.comp.revision;
    let voice_checking = {
        let hub = pane.hub.read(cx);
        ctx.tts_conn_ids
            .iter()
            .any(|cid| hub.setup.checking_connections.contains_key(cid))
    };

    div()
        .w_full()
        .min_w_0()
        .v_flex()
        .gap(px(6.))
        .child(render_voice_header_row(
            id,
            voice_checking,
            ctx,
            &pane.hub,
            theme,
        ))
        .child(dropdown_select(
            DropdownSpec {
                id: format!("comp-voice:{id}"),
                selected_value: Some(VoiceChoice {
                    connection: voice
                        .connection_id
                        .as_deref()
                        .and_then(|id| ConnectionId::parse(id).ok()),
                    voice: voice.voice_id.clone(),
                }),
                placeholder: selected_voice_label.into(),
                options: voice_options(ctx.tts_voices),
                enabled: true,
                max_list_height: 210.,
                search_placeholder: "話者名・IDで絞り込み…",
                empty_label: "有効なTTS接続から話者を取得できませんでした",
            },
            move |picked, _, cx| {
                _ = pane_voice_pick.update(cx, |pane, cx| {
                    if let Some(companion) = draft_companion.clone() {
                        pane.form
                            .voice_connections
                            .insert(companion, (revision, picked.connection));
                    }
                    voice_d_pick.update(cx, |draft, cx| {
                        draft.set(picked.voice.unwrap_or_default(), cx);
                    });
                    cx.notify();
                });
            },
        ))
        .when(ctx.tts_voices.is_empty(), |this| {
            this.child(drafts.voice_d.clone())
        })
        .child(
            div()
                .flex()
                .flex_row()
                .gap(px(8.))
                .child(div().flex_1().child(labeled_field(
                    "話速",
                    drafts.voice_speed_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "音高",
                    drafts.voice_pitch_d.clone(),
                    theme,
                )))
                .child(div().flex_1().child(labeled_field(
                    "調子",
                    drafts.voice_style_d.clone(),
                    theme,
                ))),
        )
        .into_any_element()
}

pub(super) fn render_voice_header_row(
    id: &str,
    voice_checking: bool,
    ctx: &EditorContext<'_>,
    hub: &Entity<crate::ui::Hub>,
    theme: &Theme,
) -> gpui::Div {
    let hub_refresh_voice = hub.clone();
    let refresh_tts_ids = ctx.tts_conn_ids.to_vec();
    div()
        .h_flex()
        .justify_between()
        .gap(px(8.))
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink_muted)
                .child("声（接続・話者）"),
        )
        .when(!refresh_tts_ids.is_empty(), |this| {
            this.child(pill_button(
                SharedString::from(format!("comp-voice-refresh-{id}")),
                if voice_checking {
                    "話者を取得中…"
                } else {
                    "話者一覧を取得"
                },
                Tone::Ghost
                    .icon(Icon::RefreshCw)
                    .enabled(ctx.live && !voice_checking),
                theme,
                move |_, cx| {
                    hub_refresh_voice.update(cx, |hub, cx| {
                        for cid in &refresh_tts_ids {
                            hub.check_connection(cid.clone(), cx);
                        }
                    });
                },
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_client_core::domain::{Companion, JsonDocument};

    #[test]
    fn voice_routes_keep_provider_defaults_legacy_ids_and_duplicate_speaker_names() {
        let options = voice_options(&[
            ("tts-a".into(), "shared".into(), "A".into(), "First".into()),
            ("tts-b".into(), "shared".into(), "B".into(), "Second".into()),
        ]);
        assert!(options[0].value == VoiceChoice::default());
        assert!(options[1].value != options[2].value);
        assert_eq!(
            options[2]
                .value
                .connection
                .as_ref()
                .map(ConnectionId::as_str),
            Some("tts-b")
        );
        assert_eq!(options[2].value.voice.as_deref(), Some("shared"));
        assert!(options[2].value.contains_query("tts-b"));

        let mut companion = Companion {
            voice: Some(JsonDocument {
                schema: "ene.voice.v1".into(),
                json: serde_json::json!({"connection_id":"tts-b","speed":1.2345}).to_string(),
            }),
            ..Default::default()
        };
        let binding = displayed_voice(&companion);
        assert_eq!(binding.connection_id.as_deref(), Some("tts-b"));
        assert_eq!(binding.voice_id, None);
        assert_eq!(binding.speed, Some(1.2345));
        companion.voice_identifier = Some("legacy-3".into());
        assert_eq!(
            displayed_voice(&companion).voice_id.as_deref(),
            Some("legacy-3")
        );
    }
}
