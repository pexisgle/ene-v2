//! Companion management: identity, the declared model assignment, and the
//! mind the host publishes.
//!
//! The window edits only what the host exposes. It does not infer emotion, does
//! not write memories, and does not choose a model for itself: a model is set by
//! naming a connection and a model id the host itself listed.

use super::{Desktop, SettingsSection, Tone, button, card, decode, line, muted};
use crate::update::Update;
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{AnyElement, Context, SharedString, div, prelude::*, rgb};

impl Desktop {
    /// Saves the name, persona and mind the operator edited.
    fn save_companion(&mut self, cx: &mut Context<Self>) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let name = self
            .drafts
            .get(&format!("companion:{}:name", companion.id))
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        let persona = self
            .drafts
            .get(&format!("companion:{}:persona", companion.id))
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        let mind = self
            .drafts
            .get(&format!("companion:{}:mind", companion.id))
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        self.request(
            "コンパニオンの保存",
            async move {
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: companion.id.clone(),
                        // The revision the window read is the whole point of the
                        // compare-and-swap: a silent overwrite is what the host
                        // refuses.
                        revision: companion.revision,
                        name: (!name.is_empty() && name != companion.name).then_some(name),
                        persona: Some(persona),
                        mind: (!mind.is_empty() && mind != companion.mind).then_some(mind),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |companion| {
                Update::Notification(Box::new(w::Notification {
                    kind: "companion".into(),
                    event: Some(w::notification::Event::Companion(Box::new(companion))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    fn delete_companion(&mut self, cx: &mut Context<Self>) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let id = companion.id.clone();
        self.request(
            "コンパニオンの削除",
            async move {
                host.host
                    .delete_companion(w::Id {
                        id: companion.id,
                        ..Default::default()
                    })
                    .await
            },
            move |_response| Update::Gone { companion_id: id },
        );
        cx.notify();
    }

    /// Sets one of the companion's model assignments to a model the host listed.
    fn assign(
        &mut self,
        target: AssignTarget,
        connection_id: &str,
        model_id: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let patch = w::AssignmentPatch {
            change: Some(w::assignment_patch::Change::Set(Box::new(
                w::ModelAssignment {
                    connection_id: connection_id.to_owned(),
                    model_id: model_id.to_owned(),
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let connection = connection_id.to_owned();
        let model = model_id.to_owned();
        self.request(
            "モデル割り当ての変更",
            async move {
                let mut request = w::UpdateCompanionRequest {
                    id: companion.id.clone(),
                    revision: companion.revision,
                    ..Default::default()
                };
                match target {
                    AssignTarget::Companion => request.companion_assignment = Some(patch).into(),
                    AssignTarget::Task => request.task_assignment = Some(patch).into(),
                }
                let response = host.host.update_companion(request).await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |companion| {
                let _ = (&connection, &model);
                Update::Notification(Box::new(w::Notification {
                    kind: "companion".into(),
                    event: Some(w::notification::Event::Companion(Box::new(companion))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    fn create_companion(&mut self, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let name = self
            .drafts
            .get("new:name")
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        // The persona is optional, and the host does not require a mind either:
        // a companion with a name alone is valid.
        let persona = self
            .drafts
            .get("new:mind")
            .map(|draft| draft.read(cx).text().trim().to_owned())
            .unwrap_or_default();
        let Some((connection_id, model_id)) = self.creation_model() else {
            return;
        };
        if name.is_empty() {
            return;
        }
        // The voice is whatever the speech connection offers, so a companion
        // created here speaks without a second setup step.
        let voice = self
            .model
            .settings()
            .and_then(|settings| {
                settings
                    .connections
                    .iter()
                    .find(|connection| connection.kind == w::ConnectionKind::Tts)
                    .and_then(|connection| {
                        connection
                            .config
                            .as_option()
                            .and_then(|config| config.config.as_ref())
                    })
                    .and_then(|config| match config {
                        w::connection_config::Config::Tts(tts) => tts.voice_ids.first().cloned(),
                        _ => None,
                    })
            })
            .unwrap_or_default();
        self.request(
            "コンパニオンの作成",
            async move {
                let response = host
                    .host
                    .create_companion(w::CreateCompanionRequest {
                        name,
                        mind: String::new(),
                        persona: (!persona.is_empty()).then_some(persona),
                        voice_identifier: (!voice.is_empty()).then_some(voice),
                        assignments: Some(w::ModelAssignments {
                            companion: Some(w::ModelAssignment {
                                connection_id: connection_id.clone(),
                                model_id: model_id.clone(),
                                ..Default::default()
                            })
                            .into(),
                            task: Some(w::ModelAssignment {
                                connection_id,
                                model_id,
                                ..Default::default()
                            })
                            .into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |companion| Update::Created(companion.id.clone()),
        );
        cx.notify();
    }

    /// The connection and model the new companion will be assigned to.
    fn creation_model(&self) -> Option<(String, String)> {
        self.new_model.clone()
    }

    /// The companion page: the one being talked to, and the way to make another.
    ///
    /// The user never types a connection id or a model id. Choices are the ones
    /// the host reports, as chips, and the first valid one is pre-selected so the
    /// create button is never dead for want of a click nobody can guess.
    pub(crate) fn render_companion(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let companion = self.model.companion().cloned();
        let mut body: Vec<AnyElement> = Vec::new();
        if let Some(companion) = companion.as_ref() {
            let mut draft = |field, placeholder, value: &str| {
                self.drafts
                    .entry(format!("companion:{}:{field}", companion.id))
                    .or_insert_with(|| {
                        cx.new(|cx| {
                            let mut draft = crate::input::Draft::new(cx, placeholder, "TextField");
                            draft.set(value.to_owned(), cx);
                            draft
                        })
                    })
                    .clone()
            };
            let name = draft("name", "", &companion.name);
            let persona = draft("persona", "", companion.persona.as_deref().unwrap_or(""));
            let mind = draft("mind", "", &companion.mind);
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(card())
                    .border_1()
                    .border_color(line())
                    .child(field("名前", name))
                    .child(field("口調・人物像", persona))
                    .child(div().flex().child(button(
                        cx,
                        "companion-details",
                        if self.companion_details_open {
                            "▾ 基本指示・削除"
                        } else {
                            "▸ 基本指示・削除"
                        },
                        Tone::Plain,
                        true,
                        |this, _window, cx| {
                            this.companion_details_open = !this.companion_details_open;
                            cx.notify();
                        },
                    )))
                    .when(self.companion_details_open, |this| {
                        this.child(field("基本指示（心）", mind))
                    })
                    .child(div().flex().flex_row().gap_2().child(button(
                        cx,
                        "save-companion",
                        "保存",
                        Tone::Primary,
                        self.live(),
                        |this, _window, cx| this.save_companion(cx),
                    )))
                    .into_any_element(),
            );
            body.push(section("使用するモデル"));
            body.push(self.render_assignments(companion, cx));
            if self.companion_details_open {
                body.push(
                    div()
                        .flex()
                        .child(button(
                            cx,
                            "delete-companion",
                            "コンパニオンを削除",
                            Tone::Danger,
                            self.live(),
                            |this, _window, cx| this.delete_companion(cx),
                        ))
                        .into_any_element(),
                );
            }
        } else {
            body.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_4()
                    .rounded_lg()
                    .bg(rgb(0xfffbeb))
                    .border_1()
                    .border_color(rgb(0xfde68a))
                    .child(div().flex().child(button(
                        cx,
                        "settings-create-first",
                        "コンパニオンを作成",
                        Tone::Primary,
                        self.live(),
                        |this, _window, cx| this.show_settings(SettingsSection::New, cx),
                    )))
                    .into_any_element(),
            );
        }

        pane(body)
    }

    /// The creation page: a name, an optional persona, and the model to use. It is
    /// its own page so the form is never buried under the editor.
    pub(crate) fn render_new_companion(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.model_choices();
        let mut body: Vec<AnyElement> = Vec::new();
        let new_name = self.draft(cx, "new:name", "名前（例: ミオ）", "TextField");
        let new_mind = self.draft(cx, "new:mind", "", "TextField");
        body.push(field("名前（必須）", new_name.clone()));
        body.push(field("口調・人物像（任意）", new_mind));
        // Pre-select the first reported choice once, so 作成 starts enabled.
        if !choices.iter().any(|(connection, model, _)| {
            self.new_model
                .as_ref()
                .is_some_and(|(picked_connection, picked_model)| {
                    connection == picked_connection && model == picked_model
                })
        }) {
            self.new_model = choices
                .first()
                .map(|(connection, model, _)| (connection.clone(), model.clone()));
        }
        let mut chips: Vec<AnyElement> = Vec::new();
        for (connection_id, model_id, label) in &choices {
            let selected = self.new_model.as_ref().is_some_and(|(connection, model)| {
                connection == connection_id && model == model_id
            });
            let choice = Some((connection_id.clone(), model_id.clone()));
            chips.push(button(
                cx,
                SharedString::from(format!("create-model-{connection_id}-{model_id}")),
                label,
                if selected { Tone::Primary } else { Tone::Plain },
                self.live(),
                move |this, _window, cx| {
                    this.new_model = choice.clone();
                    cx.notify();
                },
            ));
        }
        body.push(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(muted()).child("会話モデル"))
                .children(if chips.is_empty() {
                    vec![
                        div()
                            .text_xs()
                            .text_color(rgb(0x9a3412))
                            .child("選べる会話モデルがありません。")
                            .into_any_element(),
                        div()
                            .flex()
                            .child(button(
                                cx,
                                "create-open-connections",
                                "サービス接続を開く",
                                Tone::Plain,
                                true,
                                |this, _window, cx| {
                                    this.show_settings(SettingsSection::Connections, cx)
                                },
                            ))
                            .into_any_element(),
                    ]
                } else {
                    vec![
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_1()
                            .children(chips)
                            .into_any_element(),
                    ]
                })
                .into_any_element(),
        );
        let named = self
            .drafts
            .get("new:name")
            .is_some_and(|draft| !draft.read(cx).text().trim().is_empty());
        let ready = named && self.new_model.is_some() && self.live();

        body.push(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(button(
                    cx,
                    "create-companion",
                    "作成",
                    Tone::Primary,
                    ready,
                    |this, _window, cx| this.create_companion(cx),
                ))
                .into_any_element(),
        );

        pane(body)
    }

    /// Every (connection, model) the host reports for an LLM connection, with the
    /// label to show. Configured model ids win over the provider catalog, because
    /// they are what the operator actually registered.
    fn model_choices(&self) -> Vec<(String, String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for connection in &settings.connections {
            if connection.kind != w::ConnectionKind::Llm || !connection.enabled {
                continue;
            }
            let configured = match connection
                .config
                .as_option()
                .and_then(|c| c.config.as_ref())
            {
                Some(w::connection_config::Config::Llm(llm)) => llm.model_ids.clone(),
                _ => Vec::new(),
            };
            let fetched = self
                .model
                .models
                .get(&connection.id)
                .cloned()
                .unwrap_or_default();
            if configured.is_empty() {
                for model in fetched {
                    let label = if model.name.is_empty() {
                        model.id.clone()
                    } else {
                        format!("{}（{}）", model.name, model.id)
                    };
                    choices.push((connection.id.clone(), model.id, label));
                }
            } else {
                for id in configured {
                    choices.push((connection.id.clone(), id.clone(), id));
                }
            }
        }
        choices
    }
    fn render_assignments(
        &mut self,
        companion: &w::Companion,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = companion
            .assignments
            .as_option()
            .cloned()
            .unwrap_or_default();
        let connections: Vec<w::Connection> = self
            .model
            .settings()
            .map(|settings| {
                settings
                    .connections
                    .iter()
                    .filter(|connection| connection.kind == w::ConnectionKind::Llm)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        let mut rows: Vec<AnyElement> = Vec::new();
        for (label, target, assignment) in [
            (
                "会話モデル",
                AssignTarget::Companion,
                current.companion.as_option().cloned(),
            ),
            (
                "作業モデル",
                AssignTarget::Task,
                current.task.as_option().cloned(),
            ),
        ] {
            let mut selected_listed = false;
            let assigned = assignment
                .as_ref()
                .map(|assignment| format!("{}（使用中）", assignment.model_id))
                .unwrap_or_else(|| "未設定".into());
            let mut chips: Vec<AnyElement> = Vec::new();
            for connection in &connections {
                let models = self
                    .model
                    .models
                    .get(&connection.id)
                    .cloned()
                    .unwrap_or_default();
                for model in models {
                    let connection_id = connection.id.clone();
                    let model_id = model.id.clone();
                    let selected = assignment.as_ref().is_some_and(|assignment| {
                        assignment.connection_id == connection_id && assignment.model_id == model_id
                    });
                    let name = if model.name.is_empty() {
                        model_id.clone()
                    } else {
                        model.name
                    };
                    if selected {
                        selected_listed = true;
                        chips.push(
                            div()
                                .px_3()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(0xdbeafe))
                                .text_sm()
                                .child(format!("{name}（使用中）"))
                                .into_any_element(),
                        );
                        continue;
                    }
                    chips.push(button(
                        cx,
                        SharedString::from(format!("assign-{label}-{connection_id}-{model_id}")),
                        name,
                        Tone::Plain,
                        self.live(),
                        move |this, _window, cx| this.assign(target, &connection_id, &model_id, cx),
                    ));
                }
            }
            rows.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().child(label))
                    .when(!selected_listed, |this| {
                        this.child(div().text_xs().text_color(muted()).child(assigned))
                    })
                    .child(div().flex().flex_row().flex_wrap().gap_1().children(chips))
                    .into_any_element(),
            );
        }
        if connections.is_empty() {
            rows.push(
                div()
                    .text_xs()
                    .text_color(muted())
                    .child(button(
                        cx,
                        "assign-open-connections",
                        "サービス接続を開く",
                        Tone::Plain,
                        true,
                        |this, _window, cx| this.show_settings(SettingsSection::Connections, cx),
                    ))
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(card())
            .border_1()
            .border_color(line())
            .children(rows)
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum AssignTarget {
    Companion,
    Task,
}

/// A labelled text field.
pub(super) fn field(label: &str, draft: gpui::Entity<crate::input::Draft>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().child(label.to_owned()))
        .child(draft)
        .into_any_element()
}

fn section(title: &str) -> AnyElement {
    div()
        .pt_2()
        .text_sm()
        .text_color(rgb(0x44403c))
        .child(title.to_owned())
        .into_any_element()
}

/// The panel's scrolling frame.
fn pane(children: Vec<AnyElement>) -> AnyElement {
    div()
        .id("companion-pane")
        .flex()
        .flex_col()
        .flex_1()
        .gap_2()
        .p_3()
        .overflow_y_scroll()
        .children(children)
        .into_any_element()
}
