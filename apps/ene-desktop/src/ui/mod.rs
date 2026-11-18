//! Shared UI hub and window modules for the desktop companion.
//!
//! `Hub` is the single UI-thread entity holding the display copy of host state,
//! device status and async RPC helpers. Both the frameless transparent floating
//! `CompanionPod` and the standalone `SettingsStudio` window observe `Hub`.

mod companions;
mod connections;
mod device;
pub mod icons;
pub mod pod;
pub mod studio;
pub mod theme;
mod widgets;

pub use icons::EmbeddedAssets;
pub use pod::{CompanionPod, Desktop, bind, focus_chat, initial_pod_bounds};
pub use studio::SettingsStudio;

use crate::{
    session::{Connection, Options},
    state::{Chat, Model, NoticeKind},
    update::{Command, Update},
};
use buffa::view::MessageView;
use ene_rpc as w;
use gpui::{
    AppContext as _, Bounds, Context, Pixels, Size, Task, WindowBounds, WindowHandle,
    WindowOptions, px, size,
};
use std::collections::BTreeMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Inputs collected from the simplified companion creation form.
pub(crate) struct NewCompanionSpec {
    pub name: String,
    pub persona: String,
    pub mind: String,
    pub voice_identifier: Option<String>,
}

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

/// Three chapters inside `SettingsStudio`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SettingsSection {
    #[default]
    Companions,
    Connections,
    Device,
}

impl SettingsSection {
    pub const ALL: [SettingsSection; 3] = [
        SettingsSection::Companions,
        SettingsSection::Connections,
        SettingsSection::Device,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Companions => "コンパニオン",
            Self::Connections => "接続",
            Self::Device => "この端末",
        }
    }

    pub fn icon(self) -> icons::Icon {
        match self {
            Self::Companions => icons::Icon::Sparkles,
            Self::Connections => icons::Icon::Plug,
            Self::Device => icons::Icon::Sliders,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Companions => "studio-companions",
            Self::Connections => "studio-connections",
            Self::Device => "studio-device",
        }
    }
}

/// Internal host model role slots, exposed only inside progressive disclosure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelRoleSlot {
    Companion,
    Task,
    Perception,
    Compaction,
    Consolidation,
    Decision,
    Embedding,
}

impl ModelRoleSlot {
    pub const ALL: [ModelRoleSlot; 7] = [
        ModelRoleSlot::Companion,
        ModelRoleSlot::Task,
        ModelRoleSlot::Perception,
        ModelRoleSlot::Compaction,
        ModelRoleSlot::Consolidation,
        ModelRoleSlot::Decision,
        ModelRoleSlot::Embedding,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Companion => "会話モデル（主要）",
            Self::Task => "作業モデル（主要）",
            Self::Perception => "画面知覚の要約",
            Self::Compaction => "会話履歴の圧縮",
            Self::Consolidation => "記憶の整理",
            Self::Decision => "権限判定",
            Self::Embedding => "埋め込み検索",
        }
    }

    pub fn fallback_hint(self) -> &'static str {
        match self {
            Self::Companion => "自動（接続先から割当）",
            Self::Task => "自動（接続先から割当）",
            Self::Perception | Self::Compaction | Self::Consolidation => "自動（会話モデルを使用）",
            Self::Decision => "自動（作業モデルを使用）",
            Self::Embedding => "未設定（キーワード検索のみ）",
        }
    }

    pub fn current(self, assignments: &w::ModelAssignments) -> Option<w::ModelAssignment> {
        let picked = match self {
            Self::Companion => assignments.companion.as_option(),
            Self::Task => assignments.task.as_option(),
            Self::Perception => assignments.perception.as_option(),
            Self::Compaction => assignments.compaction.as_option(),
            Self::Consolidation => assignments.consolidation.as_option(),
            Self::Decision => assignments.decision.as_option(),
            Self::Embedding => assignments.embedding.as_option(),
        }?;
        (!picked.connection_id.is_empty() && !picked.model_id.is_empty()).then(|| picked.clone())
    }
}

/// Single shared state hub observed by both `CompanionPod` and `SettingsStudio`.
pub struct Hub {
    pub model: Model,
    pub options: Options,
    connection: Connection,
    commands: UnboundedSender<Command>,
    updates: UnboundedSender<Update>,
    runtime: tokio::runtime::Handle,
    pub pod_mode: PodMode,
    pub settings_section: SettingsSection,
    pub settings_window: Option<WindowHandle<SettingsStudio>>,
    pub pod_scroll_ping: u64,
    pub cleared_secret_for: Option<(String, u64)>,
    pub created_ping: u64,
    auto_assigned_revisions: BTreeMap<String, u64>,
    _pump: Task<()>,
}

impl Hub {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut Context<Self>,
        options: Options,
        connection: Connection,
        commands: UnboundedSender<Command>,
        updates_tx: UnboundedSender<Update>,
        mut updates: UnboundedReceiver<Update>,
        runtime: tokio::runtime::Handle,
        discovery: String,
    ) -> Self {
        let pump = cx.spawn(async move |this, cx| {
            while let Some(update) = updates.recv().await {
                if this.update(cx, |this, cx| this.apply(update, cx)).is_err() {
                    break;
                }
            }
        });
        Self {
            model: Model::new(discovery),
            options,
            connection,
            commands,
            updates: updates_tx,
            runtime,
            pod_mode: PodMode::MiniVoice,
            settings_section: SettingsSection::Companions,
            settings_window: None,
            pod_scroll_ping: 0,
            cleared_secret_for: None,
            created_ping: 0,
            auto_assigned_revisions: BTreeMap::new(),
            _pump: pump,
        }
    }

    pub(crate) fn theme(&self) -> theme::Theme {
        theme::Theme::strawberry_milk()
    }

    pub(crate) fn host(&self) -> Option<crate::host::Host> {
        self.connection.read().ok()?.clone()
    }

    pub(crate) fn live(&self) -> bool {
        self.model.link.is_live() && self.host().is_some()
    }

    pub(crate) fn request<T, D>(
        &self,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
    {
        self.request_with(context, future, done, failure);
    }

    pub(crate) fn request_with<T, D, E>(
        &self,
        context: &'static str,
        future: impl Future<Output = Result<T, connectrpc::ConnectError>> + Send + 'static,
        done: D,
        error: E,
    ) where
        T: Send + 'static,
        D: FnOnce(T) -> Update + Send + 'static,
        E: FnOnce(&'static str, connectrpc::ConnectError) -> Update + Send + 'static,
    {
        let updates = self.updates.clone();
        self.runtime.spawn(async move {
            let update = match future.await {
                Ok(value) => done(value),
                Err(failed) => error(context, failed),
            };
            let _ = updates.send(update);
        });
    }

    fn apply(&mut self, update: Update, cx: &mut Context<Self>) {
        match &update {
            Update::CredentialRegistered { connection_id } => {
                let next_seq = self
                    .cleared_secret_for
                    .as_ref()
                    .map_or(1, |(_, seq)| seq + 1);
                self.cleared_secret_for = Some((connection_id.clone(), next_seq));
            }
            Update::Resnapshot => self.resnapshot(),
            Update::Created(id) => {
                let created_id = id.clone();
                self.created_ping = self.created_ping.saturating_add(1);
                self.resnapshot();
                self.select_companion(&created_id, cx);
            }
            _ => {}
        }
        let conflicted = matches!(update, Update::Conflict { .. });
        if let Update::SendFailed { reason, .. } = &update {
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        let settings_changed = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(w::notification::Event::SettingsUpdated(_))
                )
        );
        let models_arrived = matches!(&update, Update::Models { .. });
        let message_created = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(w::notification::Event::MessageCreated(_))
                )
        );
        let had_companion = self.model.companion_id().is_some();
        let had_approvals = self.model.approvals().len();
        let effects = self.model.apply(update);
        let selected_now = !had_companion && self.model.companion_id().is_some();

        if settings_changed {
            let _ = self.commands.send(Command::Refresh);
            self.load_models(cx);
        }
        if settings_changed || models_arrived || selected_now {
            self.maybe_auto_assign_default_model();
        }
        if conflicted {
            self.resnapshot();
            self.refresh_active_data(cx);
        }
        if self.model.approvals().len() > had_approvals || message_created {
            self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        }
        if effects.history || selected_now {
            self.load_history(cx);
        }
        if effects.approvals || selected_now {
            self.load_approvals(cx);
        }
        if effects.mind || selected_now {
            self.load_mind(cx);
        }
        if self.live() && self.model.providers.is_none() {
            self.load_providers(cx);
        }
        if selected_now {
            self.load_models(cx);
            self.load_policy_rules(cx);
        }
        cx.notify();
    }

    fn maybe_auto_assign_default_model(&mut self) {
        if !self.live() {
            return;
        }
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        if self
            .auto_assigned_revisions
            .get(&companion.id)
            .is_some_and(|rev| *rev >= companion.revision)
        {
            return;
        }
        let Some((conn_id, model_id, _)) = self.model_choices().into_iter().next() else {
            return;
        };
        let assignments = companion.assignments.as_option();
        let needs_companion = assignments
            .and_then(|a| ModelRoleSlot::Companion.current(a))
            .is_none();
        let needs_task = assignments
            .and_then(|a| ModelRoleSlot::Task.current(a))
            .is_none();
        if !needs_companion && !needs_task {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.auto_assigned_revisions
            .insert(companion.id.clone(), companion.revision);
        let patch = w::AssignmentPatch {
            change: Some(w::assignment_patch::Change::Set(Box::new(
                w::ModelAssignment {
                    connection_id: conn_id,
                    model_id,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        };
        let mut request = w::UpdateCompanionRequest {
            id: companion.id,
            revision: companion.revision,
            ..Default::default()
        };
        if needs_companion {
            request.companion_assignment = Some(patch.clone()).into();
        }
        if needs_task {
            request.task_assignment = Some(patch).into();
        }
        self.request(
            "既定モデルの自動設定",
            async move {
                let response = host.host.update_companion(request).await?;
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
    }

    pub(crate) fn refresh_active_data(&mut self, cx: &mut Context<Self>) {
        self.load_history(cx);
        self.load_approvals(cx);
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
        self.load_policy_rules(cx);
    }

    pub(crate) fn resnapshot(&self) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "ホスト状態の取得",
            async move {
                let response = host.host.get_snapshot(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |snapshot| {
                Update::Notification(Box::new(w::Notification {
                    kind: "initial_state".into(),
                    event: Some(w::notification::Event::InitialState(Box::new(snapshot))),
                    ..Default::default()
                }))
            },
        );
    }

    pub(crate) fn select_companion(&mut self, companion_id: &str, cx: &mut Context<Self>) {
        if self.model.companion_id() == Some(companion_id) {
            return;
        }
        self.model.selected = Some(companion_id.to_owned());
        self.model.chat = Chat {
            companion_id: companion_id.to_owned(),
            loading: true,
            ..Default::default()
        };
        self.model.streaming.clear();
        self.model.transcription = None;
        let _ = self.commands.send(Command::Serve(companion_id.to_owned()));
        self.load_history(cx);
        self.load_mind(cx);
        self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        cx.notify();
    }

    pub(crate) fn load_history(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        if !self.live() {
            self.model.chat.loading = false;
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        if self.model.chat.companion_id != companion_id {
            self.model.chat = Chat {
                companion_id: companion_id.clone(),
                loading: true,
                ..Default::default()
            };
        } else {
            self.model.chat.loading = true;
        }
        let target = companion_id.clone();
        let failed_target = companion_id.clone();
        self.request_with(
            "会話履歴の取得",
            async move {
                let response = host
                    .chat
                    .get_history(w::HistoryRequest {
                        companion_id,
                        limit: Some(100),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |history| Update::History {
                companion_id: target,
                messages: history.items,
            },
            move |_, error| Update::HistoryFailed {
                companion_id: failed_target,
                message: error.to_string(),
            },
        );
    }

    pub(crate) fn send(&mut self, text: String, cx: &mut Context<Self>) -> bool {
        let request_key = uuid::Uuid::new_v4().to_string();
        self.send_keyed(text, request_key, cx)
    }

    pub(crate) fn send_keyed(
        &mut self,
        text: String,
        request_key: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.live() {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "ホストに接続されていません",
            );
            cx.notify();
            return false;
        }
        let Some(companion) = self.model.companion_id().map(str::to_owned) else {
            self.model.notice(
                NoticeKind::Failed,
                "メッセージの送信",
                "コンパニオンが選択されていません",
            );
            cx.notify();
            return false;
        };
        let Some(host) = self.host() else {
            return false;
        };
        self.model.chat.sending.push(crate::state::Sending {
            request_key: request_key.clone(),
            text: text.clone(),
        });
        tracing::info!(
            companion = %companion,
            key = %request_key,
            chars = text.chars().count(),
            "message sent"
        );
        let updates = self.updates.clone();
        self.runtime.spawn(async move {
            let outcome = match host
                .chat
                .send_message(w::SendMessageRequest {
                    companion_id: companion,
                    request_key: request_key.clone(),
                    text: text.clone(),
                    client_id: String::new(),
                    ..Default::default()
                })
                .await
            {
                Ok(response) => response
                    .view()
                    .to_owned_message()
                    .map(|acceptance| acceptance.request_id)
                    .map_err(|error| error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            let update = match outcome {
                Ok(request_id) => Update::Accepted {
                    request_key,
                    request_id,
                },
                Err(reason) => Update::SendFailed {
                    request_key,
                    text,
                    reason,
                },
            };
            let _ = updates.send(update);
        });
        self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
        cx.notify();
        true
    }

    pub(crate) fn retry_unsent(&mut self, request_key: &str, text: &str, cx: &mut Context<Self>) {
        self.model
            .chat
            .unsent
            .retain(|item| item.request_key != request_key);
        let _ = self.send_keyed(text.to_owned(), request_key.to_owned(), cx);
    }

    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::StopPlayback);
        let Some(companion) = self.model.companion_id().map(str::to_owned) else {
            cx.notify();
            return;
        };
        let Some(host) = self.host() else {
            cx.notify();
            return;
        };
        let stopping = companion.clone();
        self.request(
            "発話・生成の停止",
            async move {
                host.chat
                    .stop(w::StopRequest {
                        companion_id: companion,
                        ..Default::default()
                    })
                    .await
            },
            move |_| Update::StopRequested {
                companion_id: stopping,
            },
        );
        cx.notify();
    }

    pub(crate) fn toggle_mic_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.mic_muted;
        self.model.mic_muted = next;
        if next {
            self.model.audio_input_rms = 0.0;
        }
        let _ = self.commands.send(Command::SetMicMuted(next));
        cx.notify();
    }

    pub(crate) fn toggle_output_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.output_muted;
        self.model.output_muted = next;
        if next {
            self.model.audio_output_rms = 0.0;
        }
        let _ = self.commands.send(Command::SetOutputMuted(next));
        cx.notify();
    }

    pub(crate) fn set_avatar_scale(&mut self, scale: f32, cx: &mut Context<Self>) {
        let clamped = scale.clamp(50., 200.);
        self.model.avatar_scale = clamped;
        let _ = self.commands.send(Command::SetAvatarScale(clamped));
        cx.notify();
    }

    pub(crate) fn center_avatar(&self) {
        let _ = self.commands.send(Command::CenterAvatar);
    }

    pub(crate) fn activate_this_client(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.model.selected = Some(id.clone());
        let _ = self.commands.send(Command::Serve(id));
        cx.notify();
    }

    pub(crate) fn set_pod_mode(&mut self, mode: PodMode, cx: &mut Context<Self>) {
        if self.pod_mode != mode {
            self.pod_mode = mode;
            cx.notify();
        }
    }

    pub(crate) fn dismiss_notice(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.model.notices.len() {
            self.model.notices.remove(index);
            cx.notify();
        }
    }

    /// Opens or focuses the standalone `SettingsStudio` window at `section`.
    pub fn open_settings(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.settings_section = section;
        self.load_providers(cx);
        self.load_models(cx);
        self.load_mind(cx);
        self.load_policy_rules(cx);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _ = cx.update(|cx| {
                let Some(hub) = this.upgrade() else {
                    return;
                };
                let existing = hub.read(cx).settings_window;
                if let Some(handle) = existing
                    && handle
                        .update(cx, |_, window, cx| {
                            window.activate_window();
                            cx.notify();
                        })
                        .is_ok()
                {
                    return;
                }

                let hub_for_view = hub.clone();
                let bounds = Bounds::centered(None, size(px(780.), px(580.)), cx);
                let opened = cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(bounds)),
                        window_min_size: Some(size(px(680.), px(480.))),
                        window_decorations: Some(gpui::WindowDecorations::Client),
                        app_id: Some("ene-desktop".into()),
                        titlebar: Some(gpui::TitlebarOptions {
                            title: Some("設定 — ene".into()),
                            appears_transparent: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    move |_window, cx| cx.new(|cx| SettingsStudio::new(hub_for_view.clone(), cx)),
                );
                match opened {
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                        let _ = this.update(cx, |hub, cx| {
                            hub.settings_window = Some(handle);
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        let _ = this.update(cx, |hub, cx| {
                            hub.model.notice(
                                NoticeKind::Failed,
                                "設定ウィンドウを開く",
                                error.to_string(),
                            );
                            cx.notify();
                        });
                    }
                }
            });
        })
        .detach();
    }

    // --- Companions & Mind --------------------------------------------------

    pub(crate) fn create_companion(&mut self, spec: NewCompanionSpec, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let NewCompanionSpec {
            name,
            persona,
            mind,
            voice_identifier,
        } = spec;
        let default_model = self.model_choices().into_iter().next();
        let voice = voice_identifier
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                self.model.settings().and_then(|settings| {
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
                            w::connection_config::Config::Tts(tts) => {
                                tts.voice_ids.first().cloned()
                            }
                            _ => None,
                        })
                })
            });
        let assignments = default_model.map(|(conn_id, model_id, _)| w::ModelAssignments {
            companion: Some(w::ModelAssignment {
                connection_id: conn_id.clone(),
                model_id: model_id.clone(),
                ..Default::default()
            })
            .into(),
            task: Some(w::ModelAssignment {
                connection_id: conn_id,
                model_id,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        self.request(
            "コンパニオンの作成",
            async move {
                let response = host
                    .host
                    .create_companion(w::CreateCompanionRequest {
                        name,
                        mind,
                        persona: (!persona.trim().is_empty()).then_some(persona),
                        voice_identifier: voice,
                        assignments: assignments.into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |companion| Update::Created(companion.id.clone()),
        );
        cx.notify();
    }

    pub(crate) fn update_companion_profile(
        &mut self,
        name: String,
        persona: String,
        mind: String,
        voice_identifier: String,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "コンパニオンの保存",
            async move {
                let response = host
                    .host
                    .update_companion(w::UpdateCompanionRequest {
                        id: companion.id.clone(),
                        revision: companion.revision,
                        name: (!name.is_empty() && name != companion.name).then_some(name),
                        persona: Some(persona),
                        mind: (mind != companion.mind).then_some(mind),
                        voice_identifier: Some(voice_identifier),
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

    pub(crate) fn assign_model(
        &mut self,
        slot: ModelRoleSlot,
        assignment: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let patch = w::AssignmentPatch {
            change: Some(match assignment {
                Some((connection_id, model_id)) => {
                    w::assignment_patch::Change::Set(Box::new(w::ModelAssignment {
                        connection_id,
                        model_id,
                        ..Default::default()
                    }))
                }
                None => w::assignment_patch::Change::Clear(Box::default()),
            }),
            ..Default::default()
        };
        self.request(
            "モデル割当の変更",
            async move {
                let mut request = w::UpdateCompanionRequest {
                    id: companion.id.clone(),
                    revision: companion.revision,
                    ..Default::default()
                };
                match slot {
                    ModelRoleSlot::Companion => request.companion_assignment = Some(patch).into(),
                    ModelRoleSlot::Task => request.task_assignment = Some(patch).into(),
                    ModelRoleSlot::Perception => request.perception_assignment = Some(patch).into(),
                    ModelRoleSlot::Compaction => request.compaction_assignment = Some(patch).into(),
                    ModelRoleSlot::Consolidation => {
                        request.consolidation_assignment = Some(patch).into()
                    }
                    ModelRoleSlot::Decision => request.decision_assignment = Some(patch).into(),
                    ModelRoleSlot::Embedding => request.embedding_assignment = Some(patch).into(),
                }
                let response = host.host.update_companion(request).await?;
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

    pub(crate) fn delete_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let gone_id = companion_id.clone();
        self.request(
            "コンパニオンの削除",
            async move {
                host.host
                    .delete_companion(w::Id {
                        id: companion_id,
                        ..Default::default()
                    })
                    .await
            },
            move |_| Update::Gone {
                companion_id: gone_id,
            },
        );
        cx.notify();
    }

    pub(crate) fn load_mind(&mut self, _cx: &mut Context<Self>) {
        let Some(companion_id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "詳細プロフィールの取得",
            async move {
                let response = host
                    .host
                    .get_mind(w::Id {
                        id: companion_id,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |view| Update::MindLoaded {
                companion_id: target,
                view: Box::new(view),
            },
        );
    }

    pub(crate) fn update_mind_profile(
        &mut self,
        companion_id: String,
        expected_revision: u64,
        profile: w::MindProfile,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "詳細プロフィールの保存",
            async move {
                let response = host
                    .host
                    .update_mind_profile(w::UpdateMindProfileRequest {
                        companion_id,
                        expected_revision,
                        profile: Some(profile).into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |view| Update::MindLoaded {
                companion_id: target,
                view: Box::new(view),
            },
        );
        cx.notify();
    }

    // --- Connections, Approvals & Policy ------------------------------------

    pub(crate) fn load_providers(&mut self, _cx: &mut Context<Self>) {
        if self.model.providers.is_some() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プロバイダ一覧の取得",
            async move {
                let response = host.settings.list_providers(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |providers| Update::Providers(providers.items),
        );
    }

    pub(crate) fn load_models(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        for connection in settings
            .connections
            .iter()
            .filter(|connection| connection.kind == w::ConnectionKind::Llm)
        {
            let id = connection.id.clone();
            let request_id = id.clone();
            let host = host.clone();
            self.request(
                "モデル一覧の取得",
                async move {
                    let response = host
                        .settings
                        .list_models(w::ModelsRequest {
                            id: request_id,
                            ..Default::default()
                        })
                        .await?;
                    response.view().to_owned_message().map_err(decode)
                },
                move |models| Update::Models {
                    connection_id: id,
                    models: models.items,
                },
            );
        }
    }

    pub(crate) fn load_approvals(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "承認一覧の取得",
            async move {
                let response = host
                    .settings
                    .list_pending_approvals(w::Empty::default())
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |approvals| Update::Approvals(approvals.items),
        );
    }

    pub(crate) fn resolve_approval(
        &mut self,
        request_id: &str,
        decision: w::ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let request = request_id.to_owned();
        let resolved = request_id.to_owned();
        let stale = request_id.to_owned();
        self.request_with(
            "承認の送信",
            async move {
                host.settings
                    .submit_approval(w::SubmitApprovalRequest {
                        request_id: request,
                        decision: decision.into(),
                        ..Default::default()
                    })
                    .await
            },
            move |_| Update::Resolved {
                request_id: resolved,
            },
            move |context, error| match error.code {
                connectrpc::ErrorCode::Aborted => Update::ApprovalStale {
                    message: format!("操作確認 {stale} は期限切れか別端末で処理されました。"),
                },
                _ => failure(context, error),
            },
        );
        cx.notify();
    }

    pub(crate) fn load_policy_rules(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "許可ルールの取得",
            async move {
                let response = host.settings.list_policy_rules(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |rules| Update::PolicyRules(rules.items),
        );
    }

    pub(crate) fn add_policy_rule(
        &mut self,
        tool_name: String,
        pattern: String,
        effect: w::PolicyEffect,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let rule = w::PolicyRule {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name,
            pattern,
            effect: effect.into(),
            ..Default::default()
        };
        self.request(
            "許可ルールの追加",
            async move {
                let response = host
                    .settings
                    .update_policy_rules(w::UpdatePolicyRulesRequest {
                        add_or_update: vec![rule],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |rules| Update::PolicyRules(rules.items),
        );
        cx.notify();
    }

    pub(crate) fn remove_policy_rule(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "許可ルールの削除",
            async move {
                let response = host
                    .settings
                    .update_policy_rules(w::UpdatePolicyRulesRequest {
                        remove_ids: vec![id],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |rules| Update::PolicyRules(rules.items),
        );
        cx.notify();
    }

    pub(crate) fn upsert_connection(
        &mut self,
        context: &'static str,
        input: w::ConnectionInput,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            context,
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![input],
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |settings| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(settings).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: w::Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        self.upsert_connection(
            "接続状態の更新",
            w::ConnectionInput {
                id: connection.id,
                name: connection.name,
                enabled,
                expected_revision: connection.revision,
                kind: connection.kind,
                config: connection.config,
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_llm_from_provider(
        &mut self,
        provider: w::LlmProvider,
        model_ids: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.upsert_connection(
            "接続の追加",
            llm_provider_input(uuid::Uuid::new_v4().to_string(), provider, model_ids),
            cx,
        );
    }

    /// Creates an LLM connection, optionally registers its secret, probes it,
    /// and fetches models in one flow for first-run onboarding.
    pub(crate) fn quick_connect_llm(
        &mut self,
        provider: w::LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let conn_id = uuid::Uuid::new_v4().to_string();
        let companion = self.model.companion_id().map(str::to_owned);
        let input = llm_provider_input(conn_id.clone(), provider, Vec::new());
        let updates = self.updates.clone();
        self.runtime.spawn(async move {
            let updated = match host
                .settings
                .update_settings(w::UpdateSettingsRequest {
                    expected_revision: settings.revision,
                    connections: vec![input],
                    ..Default::default()
                })
                .await
            {
                Ok(resp) => match resp.view().to_owned_message().map_err(decode) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = updates.send(failure("接続の作成", e));
                        return;
                    }
                },
                Err(e) => {
                    let _ = updates.send(failure("接続の作成", e));
                    return;
                }
            };
            let _ = updates.send(Update::Notification(Box::new(w::Notification {
                kind: "settings".into(),
                event: Some(w::notification::Event::SettingsUpdated(Box::new(
                    w::SettingsUpdated {
                        settings: Some(updated).into(),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })));

            if !secret.trim().is_empty() {
                if let Err(e) = host
                    .settings
                    .register_credentials(w::CredentialRequest {
                        id: conn_id.clone(),
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await
                {
                    let _ = updates.send(failure("APIキーの登録", e));
                    return;
                }
                let _ = updates.send(Update::CredentialRegistered {
                    connection_id: conn_id.clone(),
                });
            }

            if let Ok(check_resp) = host
                .settings
                .check_connection(w::CheckRequest {
                    id: conn_id.clone(),
                    request_key: uuid::Uuid::new_v4().to_string(),
                    companion_id: companion,
                    ..Default::default()
                })
                .await
                && let Ok(res) = check_resp.view().to_owned_message()
            {
                let _ = updates.send(Update::Checked {
                    connection_id: conn_id.clone(),
                    check: res.check.into_option().unwrap_or_default(),
                });
            }

            if let Ok(models_resp) = host
                .settings
                .list_models(w::ModelsRequest {
                    id: conn_id.clone(),
                    ..Default::default()
                })
                .await
                && let Ok(models) = models_resp.view().to_owned_message()
            {
                let _ = updates.send(Update::Models {
                    connection_id: conn_id,
                    models: models.items,
                });
            }
        });
        cx.notify();
    }

    pub(crate) fn add_voicevox_preset(
        &mut self,
        speaker: u32,
        speed: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        let options = match speed {
            Some(speed) => format!("{{\"speaker\":{speaker},\"speed\":{speed:.2}}}"),
            None => format!("{{\"speaker\":{speaker}}}"),
        };
        self.upsert_connection(
            "VOICEVOX接続の追加",
            w::ConnectionInput {
                id: "voicevox".into(),
                name: "VOICEVOX（ローカル）".into(),
                enabled: true,
                expected_revision: 0,
                kind: w::ConnectionKind::Tts.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Tts(Box::new(
                        w::TtsConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "voicevox".into(),
                                endpoint: "http://127.0.0.1:50021".into(),
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            voice_ids: vec![speaker.to_string()],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_openai_tts_preset(
        &mut self,
        endpoint: String,
        model: String,
        voice: String,
        cx: &mut Context<Self>,
    ) {
        let options = serde_json::json!({
            "protocol": "openai",
            "model": if model.trim().is_empty() { "tts-1" } else { model.trim() },
            "voice": if voice.trim().is_empty() { "alloy" } else { voice.trim() },
        })
        .to_string();
        self.upsert_connection(
            "TTS接続の追加",
            w::ConnectionInput {
                id: uuid::Uuid::new_v4().to_string(),
                name: "OpenAI互換 TTS".into(),
                enabled: true,
                expected_revision: 0,
                kind: w::ConnectionKind::Tts.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Tts(Box::new(
                        w::TtsConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "openai".into(),
                                endpoint: if endpoint.trim().is_empty() {
                                    "https://api.openai.com/v1".into()
                                } else {
                                    endpoint.trim().to_owned()
                                },
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            voice_ids: vec![if voice.trim().is_empty() {
                                "alloy".into()
                            } else {
                                voice.trim().to_owned()
                            }],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn add_stt_preset(
        &mut self,
        name: String,
        endpoint: String,
        model: String,
        language: String,
        cx: &mut Context<Self>,
    ) {
        let options = serde_json::json!({
            "protocol": "openai",
            "model": if model.trim().is_empty() { "whisper-1" } else { model.trim() },
            "language": if language.trim().is_empty() { "ja" } else { language.trim() },
        })
        .to_string();
        let lang = if language.trim().is_empty() {
            "ja".to_owned()
        } else {
            language.trim().to_owned()
        };
        self.upsert_connection(
            "音声認識（STT）接続の追加",
            w::ConnectionInput {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                enabled: true,
                expected_revision: 0,
                kind: w::ConnectionKind::Stt.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Stt(Box::new(
                        w::SttConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "openai".into(),
                                endpoint,
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            language_hints: vec![lang],
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let id = connection_id.clone();
        let companion = self.model.companion_id().map(str::to_owned);
        self.request(
            "接続確認",
            async move {
                let response = host
                    .settings
                    .check_connection(w::CheckRequest {
                        id: connection_id,
                        request_key: uuid::Uuid::new_v4().to_string(),
                        companion_id: companion,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |result| Update::Checked {
                connection_id: id,
                check: result.check.into_option().unwrap_or_default(),
            },
        );
        cx.notify();
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: String,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        if secret.trim().is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let id = connection_id.clone();
        self.request(
            "資格情報の登録",
            async move {
                let response = host
                    .settings
                    .register_credentials(w::CredentialRequest {
                        id: connection_id,
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |_| Update::CredentialRegistered { connection_id: id },
        );
        cx.notify();
    }

    pub(crate) fn revoke_secret(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "資格情報の削除",
            async move {
                host.settings
                    .revoke_credentials(w::Id {
                        id: connection_id,
                        ..Default::default()
                    })
                    .await
            },
            |_| Update::Resnapshot,
        );
        cx.notify();
    }

    /// Discovered TTS voice identifiers from enabled TTS connections.
    pub(crate) fn tts_voice_choices(&self) -> Vec<(String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let mut voices = Vec::new();
        for conn in &settings.connections {
            if conn.kind != w::ConnectionKind::Tts || !conn.enabled {
                continue;
            }
            if let Some(check) = self
                .model
                .checks
                .get(&conn.id)
                .or_else(|| conn.last_check.as_option())
            {
                for res in &check.resources {
                    if !voices.iter().any(|(existing, _)| existing == res) {
                        voices.push((res.clone(), format!("{} / {res}", conn.name)));
                    }
                }
            }
            if let Some(w::connection_config::Config::Tts(tts)) =
                conn.config.as_option().and_then(|c| c.config.as_ref())
            {
                for id in &tts.voice_ids {
                    if !voices.iter().any(|(existing, _)| existing == id) {
                        voices.push((id.clone(), format!("{} / {id}", conn.name)));
                    }
                }
            }
        }
        voices
    }

    /// Every `(connection_id, model_id, display_label)` available across enabled LLM connections.
    pub(crate) fn model_choices(&self) -> Vec<(String, String, String)> {
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
                .and_then(|config| config.config.as_ref())
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
                        format!("{} / {}", connection.name, model.id)
                    } else {
                        format!("{} / {}", connection.name, model.name)
                    };
                    choices.push((connection.id.clone(), model.id, label));
                }
            } else {
                for id in configured {
                    choices.push((
                        connection.id.clone(),
                        id.clone(),
                        format!("{} / {id}", connection.name),
                    ));
                }
            }
        }
        choices
    }

    /// Model choices across all LLM connections for progressive-disclosure slot overrides.
    pub(crate) fn all_llm_models(&self) -> Vec<(String, String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for connection in &settings.connections {
            if connection.kind != w::ConnectionKind::Llm {
                continue;
            }
            let configured = match connection
                .config
                .as_option()
                .and_then(|config| config.config.as_ref())
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
            if fetched.is_empty() {
                for id in configured {
                    choices.push((connection.id.clone(), id.clone(), id));
                }
            } else {
                for model in fetched {
                    let name = if model.name.is_empty() {
                        model.id.clone()
                    } else {
                        model.name
                    };
                    choices.push((connection.id.clone(), model.id, name));
                }
            }
        }
        choices
    }
}

fn llm_provider_input(
    id: String,
    provider: w::LlmProvider,
    model_ids: Vec<String>,
) -> w::ConnectionInput {
    w::ConnectionInput {
        id,
        name: provider.name,
        enabled: true,
        expected_revision: 0,
        kind: w::ConnectionKind::Llm.into(),
        config: Some(w::ConnectionConfig {
            config: Some(w::connection_config::Config::Llm(Box::new(
                w::LlmConnection {
                    endpoint: Some(w::ConnectionEndpoint {
                        adapter: provider.adapter,
                        endpoint: provider.endpoint,
                        options_schema: String::new(),
                        options: "{}".into(),
                        ..Default::default()
                    })
                    .into(),
                    model_ids,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

pub(crate) fn decode(error: buffa::DecodeError) -> connectrpc::ConnectError {
    connectrpc::ConnectError::internal(error.to_string())
}

pub(crate) fn failure(context: &'static str, error: connectrpc::ConnectError) -> Update {
    match error.code {
        connectrpc::ErrorCode::Aborted => Update::Conflict {
            context,
            message: error.to_string(),
        },
        connectrpc::ErrorCode::Unauthenticated | connectrpc::ErrorCode::PermissionDenied => {
            Update::Denied {
                context,
                message: error.to_string(),
            }
        }
        _ => Update::Failed {
            context,
            message: error.to_string(),
        },
    }
}

/// Keeps a per-window draft entity map synced with the active field theme.
pub(crate) fn window_draft(
    drafts: &mut BTreeMap<String, gpui::Entity<crate::input::Draft>>,
    cx: &mut gpui::App,
    key: impl Into<String>,
    placeholder: &'static str,
    initial: Option<&str>,
    look: crate::input::FieldLook,
) -> gpui::Entity<crate::input::Draft> {
    let entity = drafts
        .entry(key.into())
        .or_insert_with(|| {
            cx.new(|cx| {
                let mut draft = crate::input::Draft::new(cx, placeholder, "TextField");
                if let Some(value) = initial {
                    draft.set(value.to_owned(), cx);
                }
                draft
            })
        })
        .clone();
    if entity.read(cx).look() != &look {
        entity.update(cx, |draft, _| draft.apply_look(look));
    }
    entity
}
