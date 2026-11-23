//! Companion editor drafts and value parsing.

//! Per-companion profile, VRM model, voice binding, and mind profile editor.

use super::super::CompanionsPane;
use crate::{
    input::Draft,
    ui::{theme::Theme, widgets::DropdownOption},
};
use ene_client_core::domain::{BodyBinding, Companion, MindView, VoiceBinding};
use gpui::{Context, Entity};

pub(crate) struct EditorContext<'a> {
    pub(crate) comp: &'a Companion,
    pub(crate) mind_view: Option<&'a MindView>,
    pub(crate) tts_voices: &'a [(String, String, String, String)],
    pub(crate) tts_conn_ids: &'a [String],
    pub(crate) exported_json: Option<&'a str>,
    pub(crate) live: bool,
}

pub(super) struct CompanionEditorDrafts {
    pub(super) name_d: Entity<Draft>,
    pub(super) mind_d: Entity<Draft>,
    pub(super) persona_d: Entity<Draft>,
    pub(super) body_model_d: Entity<Draft>,
    pub(super) voice_key_d: Entity<Draft>,
    pub(super) voice_d: Entity<Draft>,
    pub(super) voice_speed_d: Entity<Draft>,
    pub(super) voice_pitch_d: Entity<Draft>,
    pub(super) voice_style_d: Entity<Draft>,
}

pub(super) fn init_companion_editor_drafts(
    pane: &mut CompanionsPane,
    comp: &Companion,
    current_body: &BodyBinding,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> CompanionEditorDrafts {
    let id = &comp.id;
    let rev = comp.revision;
    let mut current_voice = comp
        .voice
        .as_ref()
        .and_then(|doc| serde_json::from_str::<VoiceBinding>(&doc.json).ok())
        .unwrap_or_default();
    if current_voice
        .voice_id
        .as_deref()
        .is_none_or(|v| v.trim().is_empty())
    {
        current_voice.voice_id = comp.voice_identifier.clone();
    }
    let initial_voice_key = match (
        current_voice
            .connection_id
            .as_deref()
            .filter(|s| !s.is_empty()),
        current_voice.voice_id.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(cid), Some(vid)) => format!("{cid}\0{vid}"),
        (None, Some(vid)) => format!("\0{vid}"),
        _ => String::new(),
    };
    let initial_speed = current_voice.speed.map(|v| format!("{v:.2}"));
    let initial_pitch = current_voice.pitch.map(|v| format!("{v:.2}"));

    CompanionEditorDrafts {
        name_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:name"),
            "名前",
            Some(&comp.name),
            theme,
        ),
        mind_d: pane.drafts.description_draft(
            cx,
            format!("comp:{id}:r{rev}:mind"),
            "人格の核",
            Some(&comp.mind),
            theme,
        ),
        persona_d: pane.drafts.description_draft(
            cx,
            format!("comp:{id}:r{rev}:persona"),
            "話し方",
            comp.persona.as_deref(),
            theme,
        ),
        body_model_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:body-model"),
            "VRM モデル（例: ENE.vrm / Seed-san.vrm / .vrm パス）",
            current_body.model.as_deref(),
            theme,
        ),
        voice_key_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-key"),
            "",
            Some(&initial_voice_key),
            theme,
        ),
        voice_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:voice"),
            "話者ID（任意）",
            current_voice.voice_id.as_deref(),
            theme,
        ),
        voice_speed_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-speed"),
            "速度（例: 1.0）",
            initial_speed.as_deref(),
            theme,
        ),
        voice_pitch_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-pitch"),
            "音高（例: 0.0）",
            initial_pitch.as_deref(),
            theme,
        ),
        voice_style_d: pane.drafts.draft(
            cx,
            format!("comp:{id}:r{rev}:voice-style"),
            "例: 丁寧・元気（任意）",
            current_voice.style.as_deref(),
            theme,
        ),
    }
}

pub(super) fn build_voice_options(
    tts_voices: &[(String, String, String, String)],
) -> Vec<DropdownOption> {
    let mut voice_options = vec![DropdownOption {
        value: String::new(),
        label: "接続の既定を使用".to_owned(),
        detail: None,
    }];
    for (cid, vid, label, conn_name) in tts_voices {
        voice_options.push(DropdownOption {
            value: format!("{cid}\0{vid}"),
            label: label.clone(),
            detail: Some(format!("{conn_name} / ID: {vid}")),
        });
    }
    voice_options
}

pub(super) fn format_selected_voice_label(
    tts_voices: &[(String, String, String, String)],
    picked_conn_id: Option<&str>,
    picked_voice_id: Option<&str>,
) -> Option<String> {
    match (picked_conn_id, picked_voice_id) {
        (None, None) => Some("接続の既定を使用".to_owned()),
        (Some(cid), Some(vid)) => tts_voices
            .iter()
            .find(|(c, v, _, _)| c == cid && v == vid)
            .map(|(_, v, label, cname)| format!("{label} ({cname} / ID: {v})"))
            .or_else(|| Some(format!("{cid} / {vid}"))),
        (None, Some(vid)) => tts_voices
            .iter()
            .find(|(_, v, _, _)| v == vid)
            .map(|(_, v, label, cname)| format!("{label} ({cname} / ID: {v})"))
            .or_else(|| Some(vid.to_owned())),
        (Some(cid), None) => Some(cid.to_owned()),
    }
}

pub(super) fn parse_finite_f32(draft: &Entity<Draft>, cx: &gpui::App) -> Option<f32> {
    draft
        .read(cx)
        .text()
        .trim()
        .parse::<f32>()
        .ok()
        .filter(|v| v.is_finite())
}

pub(super) fn non_empty_trimmed(draft: &Entity<Draft>, cx: &gpui::App) -> Option<String> {
    let s = draft.read(cx).text().trim().to_owned();
    (!s.is_empty()).then_some(s)
}
