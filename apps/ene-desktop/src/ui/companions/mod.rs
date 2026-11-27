//! Companions chapter in `SettingsStudio`: 3-field quick creation, 62px bordered rows,
//! 1-level hairline-divided editor, and collapsed `MindProfile` disclosure.

mod create;
mod editor;
mod rows;
pub(crate) mod voice;
mod voice_stt;

use super::hub::HubEvent;
use super::icons::Icon;
use super::theme::Theme;
use super::widgets::{ButtonStyle, Tone, pill_button};
use super::{DraftStore, Hub};
use ene_client_core::domain::{
    Companion, CompanionTemplate, ConnectionKind, MindView, ModelAssignment,
};
use ene_domain::{CompanionId, ConnectionId};
use gpui::{AnyElement, Context, Entity, FontWeight, Subscription, div, prelude::*, px};
use std::collections::BTreeMap;

/// Form and disclosure state owned by [`CompanionsPane`].
#[derive(Default)]
pub(super) struct CompanionFormState {
    pub(super) creating_companion: bool,
    pub(super) importing_template: bool,
    pub(super) preview_pending: bool,
    pub(super) expanded_companion_id: Option<String>,
    pub(super) mind_disclosure_open: bool,
    pub(super) delete_armed_companion: Option<String>,
    voice_connections: BTreeMap<CompanionId, (u64, Option<ConnectionId>)>,
}

/// Companion management chapter rendered as an independent GPUI pane entity.
pub(crate) struct CompanionsPane {
    pub(crate) hub: Entity<Hub>,
    pub(super) drafts: DraftStore,
    pub(super) form: CompanionFormState,
    initialized: bool,
    _events: Subscription,
}

impl std::fmt::Debug for CompanionsPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompanionsPane")
            .field("initialized", &self.initialized)
            .finish_non_exhaustive()
    }
}

pub(super) struct CompanionsSnapshot {
    pub(super) live: bool,
    pub(super) companions: Vec<Companion>,
    pub(super) active_id: Option<String>,
    pub(super) resident_ids: Vec<String>,
    pub(super) preview: Option<PreviewTarget>,
    pub(super) tts_voices: Vec<(String, String, String, String)>,
    pub(super) tts_conn_ids: Vec<String>,
    pub(super) minds: BTreeMap<String, MindView>,
    pub(super) templates: Vec<CompanionTemplate>,
    pub(super) exported_template: Option<(String, String)>,
    pub(super) preview_reply: Option<Result<String, ene_client_core::state::PreviewError>>,
}

#[derive(Clone)]
pub(super) struct PreviewTarget {
    assignment: ModelAssignment,
    recipient: String,
    revision: u64,
}

fn preview_target(hub: &Hub) -> Option<PreviewTarget> {
    let assignment = hub.model.dialogue_assignment()?;
    let settings = hub.model.settings()?;
    let connection = settings
        .connections
        .iter()
        .find(|connection| connection.id == assignment.connection_id)?;
    let providers = hub
        .model
        .connections
        .providers
        .as_deref()
        .unwrap_or_default();
    if !connection.can_start_chat(providers) {
        return None;
    }
    Some(PreviewTarget {
        assignment: assignment.clone(),
        recipient: hub.model.dialogue_receiver(connection)?,
        revision: settings.revision,
    })
}

impl CompanionsSnapshot {
    fn capture(hub: &Hub) -> Self {
        let tts_conn_ids: Vec<String> = hub.model.settings().map_or_default(|s| {
            s.connections
                .iter()
                .filter(|c| c.kind == ConnectionKind::Tts && c.enabled)
                .map(|c| c.id.clone())
                .collect()
        });
        Self {
            live: hub.live(),
            companions: hub
                .model
                .snapshot()
                .map_or_default(|s| s.companions.clone()),
            active_id: hub.model.companion_id().map(str::to_owned),
            resident_ids: hub.model.resident_companions(&hub.options.id),
            preview: preview_target(hub),
            tts_voices: hub.model.tts_voice_choices(),
            tts_conn_ids,
            minds: hub.model.companions.minds.clone(),
            templates: hub.model.companions.templates.clone(),
            exported_template: hub.model.companions.exported_template.clone(),
            preview_reply: hub.model.companions.preview_reply.clone(),
        }
    }
}

impl CompanionsPane {
    pub(crate) fn new(hub: Entity<Hub>, cx: &mut Context<Self>) -> Self {
        let _events = cx.subscribe(&hub, Self::on_hub_event);
        Self {
            hub,
            drafts: DraftStore::default(),
            form: CompanionFormState::default(),
            initialized: false,
            _events,
        }
    }

    fn on_hub_event(&mut self, _: Entity<Hub>, event: &HubEvent, cx: &mut Context<Self>) {
        match event {
            HubEvent::CompanionCreated(id) => {
                self.form.creating_companion = false;
                self.form.importing_template = false;
                self.form.preview_pending = false;
                self.form.expanded_companion_id = Some(id.clone());
                self.drafts
                    .clear_keys(cx, ["new:name", "new:mind", "new:persona", "import:json"]);
            }
            HubEvent::CompanionPreviewFinished => self.form.preview_pending = false,
            HubEvent::CredentialsSaved(_) => return,
        }
        cx.notify();
    }
}

impl gpui::Render for CompanionsPane {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.hub.read(cx).theme();
        let snap = CompanionsSnapshot::capture(self.hub.read(cx));
        if !self.initialized {
            self.initialized = true;
            self.form.expanded_companion_id = snap.active_id.clone();
        }
        let creating = self.form.creating_companion;
        let importing = self.form.importing_template;
        let header = render_companions_header(snap.live, creating, importing, &theme, cx);

        let import_panel = importing
            .then(|| create::render_import_panel(self, snap.live, &snap.templates, &theme, cx));
        let create_panel = creating.then(|| create::render_create_panel(self, &snap, &theme, cx));

        let mut rows: Vec<AnyElement> = snap
            .companions
            .iter()
            .map(|comp| self.render_companion_row(comp, &snap, &theme, cx))
            .collect();

        if rows.is_empty() {
            rows.push(render_empty_companions_row(creating, &theme, cx));
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(header)
            .when_some(import_panel, gpui::ParentElement::child)
            .when_some(create_panel, gpui::ParentElement::child)
            .child(div().w_full().flex().flex_col().gap(px(10.)).children(rows))
    }
}

fn render_companions_header(
    live: bool,
    creating: bool,
    importing: bool,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    let pane_toggle_create = cx.entity();
    let pane_toggle_import = cx.entity();
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.))
        .child(
            div()
                .text_size(px(22.))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.ink)
                .child("コンパニオン"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .child(pill_button(
                    "comp-toggle-import",
                    if importing {
                        "読込を閉じる"
                    } else {
                        "テンプレート読込"
                    },
                    Tone::Ghost.enabled(live),
                    theme,
                    move |_, cx| {
                        pane_toggle_import.update(cx, |this, cx| {
                            this.form.importing_template = !this.form.importing_template;
                            if this.form.importing_template {
                                this.form.creating_companion = false;
                            }
                            cx.notify();
                        });
                    },
                ))
                .child(pill_button(
                    "comp-toggle-create",
                    if creating {
                        "閉じる"
                    } else {
                        "新規作成"
                    },
                    ButtonStyle::new(if creating { Tone::Ghost } else { Tone::Primary })
                        .maybe_icon((!creating).then_some(Icon::Plus)),
                    theme,
                    move |_, cx| {
                        pane_toggle_create.update(cx, |this, cx| {
                            this.form.creating_companion = !this.form.creating_companion;
                            if this.form.creating_companion {
                                this.form.importing_template = false;
                            }
                            cx.notify();
                        });
                    },
                )),
        )
        .into_any_element()
}

fn render_empty_companions_row(
    creating: bool,
    theme: &Theme,
    cx: &mut Context<CompanionsPane>,
) -> AnyElement {
    div()
        .w_full()
        .bg(theme.bg_surface)
        .border_1()
        .border_color(theme.border)
        .rounded(crate::ui::theme::radius::surface())
        .px(px(16.))
        .py(px(18.))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .text_color(theme.ink_muted)
                .child("登録されているコンパニオンはありません。"),
        )
        .when(!creating, |this| {
            let pane = cx.entity();
            this.child(pill_button(
                "comp-empty-create",
                "新規作成",
                Tone::Primary.icon(Icon::Plus),
                theme,
                move |_, cx| {
                    pane.update(cx, |this, cx| {
                        this.form.creating_companion = true;
                        cx.notify();
                    });
                },
            ))
        })
        .into_any_element()
}

pub(super) fn split_pipe(raw: &str) -> Vec<String> {
    raw.split('|')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}
