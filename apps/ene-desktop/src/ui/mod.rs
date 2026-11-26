//! Shared UI hub and window modules for the desktop companion.
//!
//! `Hub` is the single UI-thread entity holding the display copy of host state,
//! window/tray state and async RPC coordination. Both the frameless transparent
//! floating `CompanionPod` and the standalone `SettingsStudio` window observe `Hub`.

mod companions;
mod connections;
mod device;
mod hub;
pub mod icons;
pub mod pod;
pub mod studio;
pub mod theme;
mod widgets;

pub use hub::{Hub, HubInit, open_pod_window};
pub use icons::EmbeddedAssets;
pub use pod::{
    CompanionPod, Desktop, bind, focus_chat, initial_pod_bounds, initial_pod_bounds_for_mode,
};
pub use studio::SettingsStudio;

pub(crate) use ene_client_core::domain::{CompanionProfilePatch, NewCompanionSpec};
pub(crate) use ene_client_core::presets::SearchPresetSpec;
pub(crate) use ene_client_core::{
    GOOGLE_AGENT_PLATFORM, ModelRoleSlot, default_embedding_dimensions, search_adapter_requires_key,
};

use gpui::{AppContext as _, Entity, Pixels, Size, px, size};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct HistoryReset {
    pub requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub failure: Option<String>,
}

impl gpui::Global for HistoryReset {}

/// Display mode of the frameless floating companion pod.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PodMode {
    #[default]
    MiniVoice,
    ExpandedChat,
}

impl PodMode {
    pub fn size(self) -> Size<Pixels> {
        match self {
            Self::MiniVoice => size(px(444.), px(218.)),
            Self::ExpandedChat => size(px(444.), px(540.)),
        }
    }

    pub fn min_size(self) -> Size<Pixels> {
        match self {
            Self::MiniVoice => size(px(360.), px(188.)),
            Self::ExpandedChat => size(px(380.), px(380.)),
        }
    }

    pub fn toggle(self) -> Self {
        match self {
            Self::MiniVoice => Self::ExpandedChat,
            Self::ExpandedChat => Self::MiniVoice,
        }
    }
}

/// The chapters inside `SettingsStudio`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    #[default]
    Companions,
    Connections,
    Voice,
    Device,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 4] = [
        SettingsSection::Companions,
        SettingsSection::Connections,
        SettingsSection::Voice,
        SettingsSection::Device,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Companions => "コンパニオン",
            Self::Connections => "接続",
            Self::Voice => "音声テスト",
            Self::Device => "この端末",
        }
    }

    pub fn icon(self) -> icons::Icon {
        match self {
            Self::Companions => icons::Icon::Sparkles,
            Self::Connections => icons::Icon::Plug,
            Self::Voice => icons::Icon::Mic,
            Self::Device => icons::Icon::Sliders,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Companions => "studio-companions",
            Self::Connections => "studio-connections",
            Self::Voice => "studio-voice",
            Self::Device => "studio-device",
        }
    }
}

/// Per-pane cache of `Draft` text inputs synced with the active field theme.
#[derive(Default)]
pub(crate) struct DraftStore {
    drafts: BTreeMap<String, Entity<crate::input::Draft>>,
}

impl DraftStore {
    pub(crate) fn draft(
        &mut self,
        cx: &mut gpui::App,
        key: impl Into<String>,
        placeholder: &'static str,
        initial: Option<&str>,
        theme: &theme::Theme,
    ) -> Entity<crate::input::Draft> {
        window_draft(
            &mut self.drafts,
            cx,
            DraftSpec {
                key: key.into(),
                placeholder,
                initial,
                look: theme.field_look(false),
                multiline: false,
            },
        )
    }

    pub(crate) fn description_draft(
        &mut self,
        cx: &mut gpui::App,
        key: impl Into<String>,
        placeholder: &'static str,
        initial: Option<&str>,
        theme: &theme::Theme,
    ) -> Entity<crate::input::Draft> {
        window_draft(
            &mut self.drafts,
            cx,
            DraftSpec {
                key: key.into(),
                placeholder,
                initial,
                look: theme.field_look(false),
                multiline: true,
            },
        )
    }

    pub(crate) fn masked_draft(
        &mut self,
        cx: &mut gpui::App,
        key: impl Into<String>,
        placeholder: &'static str,
        masked: bool,
        theme: &theme::Theme,
    ) -> Entity<crate::input::Draft> {
        let entity = self.draft(cx, key, placeholder, None, theme);
        entity.update(cx, |draft, cx| draft.set_masked(masked, cx));
        entity
    }

    pub(crate) fn clear_keys<'a>(
        &mut self,
        cx: &mut gpui::App,
        keys: impl IntoIterator<Item = &'a str>,
    ) {
        for key in keys {
            if let Some(d) = self.drafts.get(key).cloned() {
                d.update(cx, super::input::Draft::clear);
            }
        }
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.drafts.retain(|key, _| keep(key));
    }
}

/// Configuration for fetching or initializing a draft input in [`window_draft`].
pub(crate) struct DraftSpec<'a> {
    pub key: String,
    pub placeholder: &'static str,
    pub initial: Option<&'a str>,
    pub look: crate::input::FieldLook,
    pub multiline: bool,
}

/// Keeps a per-window draft entity map synced with the active field theme.
pub(crate) fn window_draft(
    drafts: &mut BTreeMap<String, Entity<crate::input::Draft>>,
    cx: &mut gpui::App,
    spec: DraftSpec<'_>,
) -> Entity<crate::input::Draft> {
    let entity = drafts
        .entry(spec.key)
        .or_insert_with(|| {
            cx.new(|cx| {
                let mut draft = crate::input::Draft::new(spec.placeholder, spec.multiline);
                if let Some(value) = spec.initial {
                    draft.set(value.to_owned(), cx);
                }
                draft
            })
        })
        .clone();
    entity.update(cx, |draft, cx| draft.apply_look(spec.look, cx));
    entity
}
