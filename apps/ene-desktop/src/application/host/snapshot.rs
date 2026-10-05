use super::super::*;
use ene_api_types::host::{Message, Snapshot};

impl Application {
    pub(super) fn project_snapshot(
        &self,
        ui: &AppWindow,
        snapshot: Snapshot,
        messages: Vec<Message>,
    ) -> bool {
        let state = ui.global::<UiState>();

        self.snapshot.replace(Some(snapshot.clone()));
        self.history.replace(messages.clone());
        state.set_companion_names(
            Rc::new(VecModel::from(
                snapshot
                    .companions
                    .iter()
                    .map(|c| {
                        if snapshot
                            .companions
                            .iter()
                            .filter(|other| other.name == c.name)
                            .count()
                            > 1
                        {
                            format!("{} · {}", c.name, c.id).into()
                        } else {
                            c.name.clone().into()
                        }
                    })
                    .collect::<Vec<slint::SharedString>>(),
            ))
            .into(),
        );
        if state.get_selected_companion_id().is_empty()
            && let Some(c) = snapshot.companions.first()
        {
            state.set_selected_companion_id(c.id.clone().into());
        }
        let selected_id = state.get_selected_companion_id();
        let index = snapshot
            .companions
            .iter()
            .position(|c| c.id == selected_id.as_str());
        state.set_companion_index(index.map_or(-1, |i| i as i32));
        let Some(c) = index.map(|i| &snapshot.companions[i]) else {
            state.set_chat_ready(false);
            state.set_can_send(false);
            state.set_has_older(false);
            state.set_selected_companion_name("".into());
            state.set_setup_hint(
                "会話相手が見つかりません。コンパニオンを選び直してください。".into(),
            );
            self.messages.set_vec(vec![]);
            return false;
        };
        state.set_selected_companion_name(c.name.clone().into());
        let ready = c.availability["chat"]["ready"] == true;
        if state.get_host_connected() && state.get_chat_ready() != ready {
            state.set_status(
                if ready {
                    "対話できます。"
                } else {
                    "対話の準備が必要です。設定で接続・資格情報・送信許可を確認してください。"
                }
                .into(),
            );
        }
        state.set_chat_ready(ready);
        let (hint, category) =
            setup_guidance(c.availability["chat"]["reasons"][0]["code"].as_str());
        state.set_setup_hint(hint.into());
        state.set_setup_category(category);
        state.set_has_older(
            snapshot
                .message_cursors
                .get(&c.id)
                .is_some_and(|c| c.is_some()),
        );
        state.set_connection_configured(!snapshot.settings.connections.is_empty());
        let assignment = c.model_assignments.companion.as_ref();
        let selected = selected_connection(
            &snapshot.settings.connections,
            state.get_selected_connection_id().as_str(),
            assignment,
            state.get_connection_dirty(),
        );
        state.set_saved_model(
            selected.is_some_and(|p| assignment.is_some_and(|a| a.connection_id == p.id)),
        );
        state.set_selected_connection_id(selected.map(|p| p.id.clone()).unwrap_or_default().into());
        state.set_credential_state(match selected.map(|p| p.credential_state.as_str()) {
            Some("stored") => 1,
            Some("session_only") => 2,
            _ => 0,
        });
        if !state.get_connection_dirty() && selected.is_none() && state.get_provider_id().is_empty()
        {
            self.project_provider(ui, 0, true);
        }
        if !state.get_connection_dirty()
            && let Some(p) = selected
        {
            state.set_provider_id(p.adapter.clone().into());
            if let Some(index) = self
                .providers
                .borrow()
                .iter()
                .position(|d| d.id == p.adapter)
            {
                self.project_provider(ui, index as i32, false);
            }
            state.set_provider_endpoint(p.endpoint.clone().into());
            state.set_model_id(
                assignment
                    .filter(|a| a.connection_id == p.id)
                    .map(|a| a.model_id.as_str())
                    .unwrap_or_default()
                    .into(),
            );
            let fields = state.get_provider_fields();
            for (row, mut f) in fields.iter().enumerate() {
                if let Some(v) = p.options.get(f.key.as_str()) {
                    f.value = v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                        .into();
                    f.checked = v.as_bool().unwrap_or(false);
                    fields.set_row_data(row, f);
                }
            }
        }
        if !state.get_companion_dirty() {
            state.set_companion_name(c.name.clone().into());
            state.set_companion_mind(c.mind.clone().into());
        }
        if !state.get_permissions_dirty() {
            state.set_allow_conversation(snapshot.grants.iter().any(|g| {
                g.status == "active"
                    && g.companion_id == c.id
                    && g.purpose == "conversation"
                    && selected.is_some_and(|p| p.id == g.connection_id)
            }));
            state.set_allow_check(snapshot.grants.iter().any(|g| {
                g.status == "active"
                    && g.companion_id == c.id
                    && g.purpose == "connection_check"
                    && selected.is_some_and(|p| p.id == g.connection_id)
            }));
        }
        if !state.get_connection_dirty() {
            let mut drafts = self.host_drafts.borrow_mut();
            drafts.settings_revision = snapshot.settings.revision;
            drafts.assignment_revision = c.revision;
            drop(drafts);
            state.set_connection_matches_provider(selected.is_some_and(|p| {
                p.adapter == state.get_provider_id().as_str()
                    && p.endpoint == state.get_provider_endpoint().as_str()
            }));
        }
        if !state.get_companion_dirty() {
            self.host_drafts.borrow_mut().companion_revision = c.revision;
        }
        self.project_permission_scope(ui);
        self.project_model_info(ui);
        let entries = messages
            .into_iter()
            .filter(|message| message.companion_id == c.id)
            .map(|m| {
                use ene_api_types::host::{MessageStatus, Role};
                let author = if m.role == Role::User {
                    "あなた".to_owned()
                } else if m.role == Role::Companion {
                    c.name.clone()
                } else {
                    "未対応の発言者".into()
                };
                let message_state = match m.status {
                    MessageStatus::Queued | MessageStatus::Streaming => 1,
                    MessageStatus::Failed => 2,
                    MessageStatus::Cancelled => 3,
                    MessageStatus::Unknown(_) => 2,
                    _ => 0,
                };
                let mut body = m.text;
                if let Some(failure) = m.failure {
                    body.push_str(&format!("\n{}", failure.message));
                }
                ChatEntry {
                    author: author.into(),
                    body: body.into(),
                    provisional: false,
                    state: message_state,
                    outgoing: m.role == Role::User,
                }
            })
            .collect::<Vec<_>>();
        self.messages.set_vec(entries);

        true
    }
}

fn setup_guidance(reason: Option<&str>) -> (&'static str, i32) {
    match reason {
        Some("storage_error") => (
            "会話の終了状態を保存できません。保存先を確認してホストを再起動してください。",
            2,
        ),
        Some("permission_required") => ("会話の送信許可を設定してください。", 3),
        Some("credential_missing") => ("AIサービスのAPIキーを登録してください。", 2),
        Some("provider_unavailable") => ("AIプロバイダーを有効にしてください。", 2),
        Some("model_unset") => ("会話に使うAIとモデルを設定してください。", 2),
        _ => ("AI接続・モデル・送信許可を確認してください。", 2),
    }
}

/// A notification must not turn a draft for a new connection into an existing edit.
pub(crate) fn selected_connection<'a>(
    connections: &'a [ene_api_types::Connection],
    id: &str,
    assignment: Option<&ene_api_types::ModelAssignment>,
    editing: bool,
) -> Option<&'a ene_api_types::Connection> {
    connections.iter().find(|p| p.id == id).or_else(|| {
        if editing {
            return None;
        }
        assignment
            .and_then(|a| connections.iter().find(|p| p.id == a.connection_id))
            .or_else(|| connections.first())
    })
}
