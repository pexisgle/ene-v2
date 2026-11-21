//! Shared UI hub and window modules for the desktop companion.
//!
//! `Hub` is the single UI-thread entity holding the display copy of host state,
//! device status and async RPC helpers. Both the frameless transparent floating
//! `CompanionPod` and the standalone `SettingsStudio` window observe `Hub`.

mod companions;
mod connections;
mod device;
pub mod icons;
mod mcp_add;
pub mod pod;
pub mod studio;
pub mod theme;
mod voice;
mod widgets;
mod workflows;

pub use icons::EmbeddedAssets;
pub use pod::{
    CompanionPod, Desktop, bind, focus_chat, initial_pod_bounds, initial_pod_bounds_for_mode,
};
pub use studio::SettingsStudio;

use crate::{
    session::{Connection, Options},
    state::{Chat, Device, Link, Model, NoticeKind},
    tray::{TrayAction, TrayHandle, TrayState},
    update::{Command, Update},
};
use buffa::view::MessageView;
pub(crate) use ene_catalog::{
    GOOGLE_AGENT_PLATFORM, OPENAI_TTS_VOICES, SAKURA_AI_ENDPOINT as SAKURA_ENDPOINT,
    SAKURA_EMBEDDING_MODELS, SAKURA_STT_MODELS, SAKURA_TTS_VOICES, SearchConnectionSpec,
    active_stt_in_connections, active_tts_in_connections, apply_stt_model_to_config,
    apply_tts_voice_to_config, build_stt_assignment_inputs, build_tts_assignment_inputs,
    can_start_chat, default_embedding_dimensions, default_models_for_adapter,
    is_local_voicevox_conn, is_sakura_conn, is_sakura_endpoint, llm_endpoint, llm_provider_input,
    needs_key, parse_voice_resource, search_adapter_of, search_adapter_requires_key,
    search_connection_input, tts_voice_of_connection,
};
use ene_rpc as w;
use gpui::{
    App, AppContext as _, Bounds, Context, Entity, Pixels, Size, Task, WeakEntity, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, px, size,
};
use std::collections::{BTreeMap, HashSet};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug)]
pub struct HistoryReset {
    pub requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub failure: Option<String>,
}

impl gpui::Global for HistoryReset {}

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
            Self::Perception => "目（画面・活動の評価）",
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
    pub pod_window: Option<WindowHandle<CompanionPod>>,
    pod_view: Option<WeakEntity<CompanionPod>>,
    pod_visible: bool,
    pub pod_scroll_ping: u64,
    pub cleared_secret_for: Option<(String, u64)>,
    pub created_ping: u64,
    setup_pending: bool,
    setup_error: Option<&'static str>,
    pub setup_error_detail: Option<String>,
    pub pending_setup_connection_id: Option<String>,
    pub pending_setup_models: Vec<String>,
    pub checking_connections: HashSet<String>,
    auto_assigned_revisions: BTreeMap<String, u64>,
    request_seq: std::sync::atomic::AtomicU64,
    tray: Option<TrayHandle>,
    _tray_pump: Option<Task<()>>,
    _pump: Task<()>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub")
            .field("pod_mode", &self.pod_mode)
            .field("settings_section", &self.settings_section)
            .field("pod_visible", &self.pod_visible)
            .finish_non_exhaustive()
    }
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
        tray: Option<TrayHandle>,
        tray_rx: Option<UnboundedReceiver<TrayAction>>,
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
        let tray_pump = tray_rx.map(|mut actions| {
            cx.spawn(async move |this, cx| {
                while let Some(action) = actions.recv().await {
                    if this
                        .update(cx, |this, cx| this.handle_tray_action(action, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
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
            pod_window: None,
            pod_view: None,
            pod_visible: true,
            pod_scroll_ping: 0,
            cleared_secret_for: None,
            created_ping: 0,
            setup_pending: false,
            setup_error: None,
            setup_error_detail: None,
            pending_setup_connection_id: None,
            pending_setup_models: Vec::new(),
            checking_connections: HashSet::new(),
            auto_assigned_revisions: BTreeMap::new(),
            request_seq: std::sync::atomic::AtomicU64::new(1),
            tray,
            _tray_pump: tray_pump,
            _pump: pump,
        }
    }

    pub fn is_tray_resident(&self) -> bool {
        self.tray.as_ref().is_some_and(TrayHandle::is_resident)
    }

    pub fn is_pod_visible(&self) -> bool {
        self.pod_visible && self.pod_window.is_some()
    }

    pub(crate) fn pod_view(&self) -> Option<WeakEntity<CompanionPod>> {
        self.pod_view.clone()
    }

    pub(crate) fn register_pod_window(
        &mut self,
        handle: WindowHandle<CompanionPod>,
        view: WeakEntity<CompanionPod>,
        cx: &mut Context<Self>,
    ) {
        self.pod_window = Some(handle);
        self.pod_view = Some(view);
        self.pod_visible = true;
        self.sync_tray_state();
        cx.notify();
    }

    pub fn hide_pod(&mut self, cx: &mut Context<Self>) {
        self.pod_visible = false;
        pod::set_pod_window_visible(false);
        self.sync_tray_state();
        cx.notify();
    }

    pub fn open_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        if let Some(next) = mode {
            self.pod_mode = next;
            if next == PodMode::ExpandedChat {
                self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
            }
        }
        self.pod_visible = true;
        let existing = self.pod_window;
        self.sync_tray_state();
        cx.notify();

        cx.spawn(async move |this, cx| {
            let _ = cx.update(|cx| {
                let Some(hub) = this.upgrade() else {
                    return;
                };
                if let Some(handle) = existing
                    && handle
                        .update(cx, |view, window, cx| {
                            pod::set_pod_window_visible(true);
                            view.sync_mode_and_focus(window, cx);
                            window.activate_window();
                            cx.activate(true);
                            cx.notify();
                        })
                        .is_ok()
                {
                    return;
                }
                if open_pod_window(cx, hub).is_some() {
                    cx.activate(true);
                }
            });
        })
        .detach();
    }

    pub fn toggle_pod(&mut self, mode: Option<PodMode>, cx: &mut Context<Self>) {
        if self.is_pod_visible() && mode.is_none_or(|m| m == self.pod_mode) {
            self.hide_pod(cx);
        } else {
            self.open_pod(mode, cx);
        }
    }

    pub fn sync_window_closed(&mut self, cx: &mut App) {
        let windows = cx.windows();
        if self
            .settings_window
            .is_some_and(|handle| !windows.contains(&handle.into()))
        {
            self.settings_window = None;
        }
        if self
            .pod_window
            .is_some_and(|handle| !windows.contains(&handle.into()))
        {
            self.pod_window = None;
            self.pod_view = None;
            self.pod_visible = false;
        }
        self.sync_tray_state();
        if windows.is_empty() || (!self.is_tray_resident() && self.pod_window.is_none()) {
            cx.quit();
        }
    }

    fn sync_tray_state(&self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let name = self
            .model
            .companion()
            .map(|c| c.name.as_str())
            .unwrap_or("ene");
        let status = match &self.model.link {
            Link::Live { .. } if !self.model.approvals().is_empty() => "確認待ち",
            Link::Live { .. } if self.model.work_active => "作業中",
            Link::Live { .. } if self.model.companion_speaking => "発話中",
            Link::Live { .. } if !self.model.streaming.is_empty() => "生成中",
            Link::Live { .. } => "待機中",
            Link::Connecting { .. } => "接続中",
            _ => "未接続",
        };
        tray.update(TrayState {
            pod_open: self.is_pod_visible(),
            pod_mode: self.pod_mode,
            mic_muted: self.model.mic_muted,
            avatar_ready: matches!(self.model.devices.avatar, Device::Ready(_)),
            tooltip: format!("{name} — {status}"),
        });
    }

    fn handle_tray_action(&mut self, action: TrayAction, cx: &mut Context<Self>) {
        match action {
            TrayAction::ActivatePod => self.open_pod(None, cx),
            TrayAction::OpenChat => self.open_pod(Some(PodMode::ExpandedChat), cx),
            TrayAction::OpenMiniPod => self.open_pod(Some(PodMode::MiniVoice), cx),
            TrayAction::HidePod => self.hide_pod(cx),
            TrayAction::OpenSettings(section) => self.open_settings(section, cx),
            TrayAction::CenterAvatar => self.center_avatar(),
            TrayAction::ToggleMicMute => self.toggle_mic_mute(cx),
            TrayAction::Quit => cx.quit(),
        }
    }

    pub(crate) fn theme(&self) -> theme::Theme {
        theme::Theme::strawberry_milk()
    }

    pub(crate) fn host(&self) -> Option<crate::host::Host> {
        self.connection.read().ok()?.clone()
    }

    pub(crate) fn has_audio_connection(&self, kind: w::ConnectionKind) -> bool {
        self.model
            .settings()
            .is_some_and(|s| s.connections.iter().any(|c| c.enabled && c.kind == kind))
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
        let epoch = self.model.link_epoch;
        let seq = self
            .request_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.runtime.spawn(async move {
            let inner = match future.await {
                Ok(value) => done(value),
                Err(failed) => error(context, failed),
            };
            let _ = updates.send(Update::Scoped {
                epoch,
                seq,
                inner: Box::new(inner),
            });
        });
    }

    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    fn apply(&mut self, mut update: Update, cx: &mut Context<Self>) {
        if let Update::Scoped {
            epoch,
            seq: _,
            inner,
        } = update
        {
            if epoch != self.model.link_epoch {
                return;
            }
            update = *inner;
        }
        match &update {
            Update::CredentialRegistered { connection_id } => {
                let next_seq = self
                    .cleared_secret_for
                    .as_ref()
                    .map_or(1, |(_, seq)| seq + 1);
                self.cleared_secret_for = Some((connection_id.clone(), next_seq));
            }
            Update::Checked {
                connection_id: id,
                check,
            } => {
                self.checking_connections.remove(id);
                if check.healthy
                    && self.model.settings().is_some_and(|s| {
                        s.connections
                            .iter()
                            .any(|c| &c.id == id && c.kind == w::ConnectionKind::Llm)
                    })
                {
                    self.setup_error = None;
                    self.setup_error_detail = None;
                }
            }
            Update::SetupFinished {
                ok,
                connection_id,
                models,
                error,
            } => {
                self.setup_pending = false;
                if *ok {
                    self.setup_error = None;
                    self.setup_error_detail = None;
                    self.pending_setup_connection_id = connection_id.clone();
                    self.pending_setup_models = models.clone();
                } else {
                    self.setup_error = Some(
                        "接続を確認できませんでした。選んだサービスのキーとネットワークを確認して、もう一度登録してください。",
                    );
                    self.setup_error_detail = error.clone();
                }
                self.resnapshot();
                if *ok {
                    self.load_models(cx);
                }
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
        if let Update::Checked {
            connection_id: _,
            check,
        } = &update
            && check.healthy
        {
            self.load_models(cx);
        }
        if let Update::Failed { context, .. }
        | Update::Conflict { context, .. }
        | Update::Denied { context, .. } = &update
            && *context == "接続確認"
        {
            self.checking_connections.clear();
        }
        if let Update::SendFailed { reason, .. } = &update {
            tracing::warn!(reason = %reason, "the message was not accepted");
        }
        let history_arrived = matches!(&update, Update::History { .. });
        let settings_changed = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(w::notification::Event::SettingsUpdated(_))
                )
        );
        let companion_or_snapshot = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(
                        w::notification::Event::InitialState(_)
                            | w::notification::Event::Companion(_)
                            | w::notification::Event::SettingsUpdated(_)
                    )
                )
        );
        let models_or_check = matches!(
            &update,
            Update::Providers(_) | Update::Models { .. } | Update::Checked { .. }
        );
        let message_arrived = matches!(
            &update,
            Update::Notification(notification)
                if matches!(
                    notification.event,
                    Some(
                        w::notification::Event::MessageCreated(_)
                            | w::notification::Event::MessageDelta(_)
                    )
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
        if companion_or_snapshot || models_or_check || selected_now {
            self.maybe_auto_assign_default_model();
        }
        if conflicted {
            self.resnapshot();
            self.refresh_active_data(cx);
        }
        if self.model.approvals().len() > had_approvals || message_arrived || history_arrived {
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
            self.load_policy_texts(cx);
        }
        self.sync_tray_state();
        cx.notify();
    }

    fn maybe_auto_assign_default_model(&mut self) {
        if !self.live() || self.setup_pending || self.model.providers.is_none() {
            return;
        }
        let Some(companion) = self.model.companion().cloned() else {
            return;
        };
        if self.pending_setup_connection_id.is_some() {
            return;
        }
        if self
            .auto_assigned_revisions
            .get(&companion.id)
            .is_some_and(|rev| *rev >= companion.revision)
        {
            return;
        }
        let Some(settings) = self.model.settings() else {
            return;
        };
        let providers = self.model.providers.as_deref().unwrap_or_default();
        let enabled_llm_ids: HashSet<&str> = settings
            .connections
            .iter()
            .filter(|c| can_start_chat(c, providers))
            .map(|c| c.id.as_str())
            .collect();
        let Some((conn_id, model_id, _)) = self
            .model_choices()
            .into_iter()
            .find(|(id, _, _)| enabled_llm_ids.contains(id.as_str()))
        else {
            return;
        };
        let assignments = companion.assignments.as_option();
        let is_valid = |slot: ModelRoleSlot| -> bool {
            assignments
                .and_then(|a| slot.current(a))
                .is_some_and(|assigned| {
                    !assigned.connection_id.is_empty()
                        && !assigned.model_id.is_empty()
                        && settings.connections.iter().any(|c| {
                            c.id == assigned.connection_id
                                && c.enabled
                                && c.kind == w::ConnectionKind::Llm
                                && !needs_key(c, providers)
                        })
                })
        };
        let needs_companion = !is_valid(ModelRoleSlot::Companion);
        let needs_task = !is_valid(ModelRoleSlot::Task);
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
        self.load_policy_texts(cx);
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
        let epoch = self.model.link_epoch;
        let seq = self
            .request_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            let _ = updates.send(Update::Scoped {
                epoch,
                seq,
                inner: Box::new(update),
            });
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

    pub(crate) fn dismiss_unsent(&mut self, request_key: &str, cx: &mut Context<Self>) {
        self.model
            .chat
            .unsent
            .retain(|item| item.request_key != request_key);
        cx.notify();
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
            self.model.transcription = None;
        }
        let _ = self.commands.send(Command::SetMicMuted(next));
        self.sync_tray_state();
        cx.notify();
    }

    pub(crate) fn toggle_capture_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self.model.capture_mode {
            crate::audio::CaptureMode::Vad => crate::audio::CaptureMode::Ptt,
            crate::audio::CaptureMode::Ptt => crate::audio::CaptureMode::Vad,
        };
        self.model.capture_mode = next;
        self.model.ptt_active = false;
        let _ = self.commands.send(Command::SetCaptureMode(next));
        cx.notify();
    }

    pub(crate) fn ptt_toggle(&mut self, cx: &mut Context<Self>) {
        if self.model.ptt_active {
            self.model.ptt_active = false;
            let _ = self.commands.send(Command::PttCommit);
        } else {
            self.model.ptt_active = true;
            let _ = self.commands.send(Command::PttStart);
        }
        cx.notify();
    }

    pub(crate) fn ptt_cancel(&mut self, cx: &mut Context<Self>) {
        self.model.ptt_active = false;
        let _ = self.commands.send(Command::PttCancel);
        cx.notify();
    }

    pub(crate) fn set_eyes_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::SetEyesEnabled(enabled));
    }

    #[cfg(windows)]
    pub(crate) fn set_computer_use_enabled(&self, enabled: bool) {
        let _ = self.commands.send(Command::SetComputerUseEnabled(enabled));
    }

    pub(crate) fn toggle_output_mute(&mut self, cx: &mut Context<Self>) {
        let next = !self.model.output_muted;
        self.model.output_muted = next;
        if next {
            self.model.audio_output_rms = 0.0;
        }
        let _ = self.commands.send(Command::SetOutputMuted(next));
        self.sync_tray_state();
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

    /// Speaks typed text through the host's TTS connection, outside any turn.
    pub(crate) fn speak_probe(&mut self, text: String, cx: &mut Context<Self>) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.model.speech_report = None;
        let companion_id = self.model.companion_id().map(str::to_owned);
        let _ = self.commands.send(Command::Speak { companion_id, text });
        cx.notify();
    }

    /// Forgets the speech probe's readback and recognized lines.
    pub(crate) fn clear_voice_probe(&mut self, cx: &mut Context<Self>) {
        self.model.recognized.clear();
        self.model.speech_report = None;
        cx.notify();
    }

    pub(crate) fn activate_this_client(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.model.companion_id().map(str::to_owned) else {
            return;
        };
        self.model.selected = Some(id.clone());
        let _ = self.commands.send(Command::Serve(id));
        cx.notify();
    }

    pub(crate) fn activate_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::ActivateCompanion(companion_id));
        cx.notify();
    }

    pub(crate) fn release_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let _ = self.commands.send(Command::ReleaseCompanion(companion_id));
        cx.notify();
    }

    pub(crate) fn set_pod_mode(&mut self, mode: PodMode, cx: &mut Context<Self>) {
        if self.pod_mode != mode {
            self.pod_mode = mode;
            if mode == PodMode::ExpandedChat {
                self.pod_scroll_ping = self.pod_scroll_ping.saturating_add(1);
            }
            self.sync_tray_state();
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
        self.load_companion_templates(cx);
        self.load_policy_rules(cx);
        self.load_plugins(cx);
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
                let bounds = settings_window_bounds(cx);
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn update_companion_profile(
        &mut self,
        companion_id: String,
        name: String,
        persona: String,
        mind: String,
        body: w::BodyBinding,
        voice: w::VoiceBinding,
        cx: &mut Context<Self>,
    ) {
        let Some(companion) = self
            .model
            .snapshot()
            .and_then(|s| s.companions.iter().find(|c| c.id == companion_id))
            .cloned()
        else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let voice_identifier = voice.voice_id.clone().unwrap_or_default();
        let body_doc = serde_json::to_string(&body)
            .ok()
            .map(|json| w::JsonDocument {
                schema: "ene.body.v1".into(),
                json,
                ..Default::default()
            });
        let voice_doc = serde_json::to_string(&voice)
            .ok()
            .map(|json| w::JsonDocument {
                schema: "ene.voice.v1".into(),
                json,
                ..Default::default()
            });
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
                        body: body_doc.into(),
                        voice: voice_doc.into(),
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

    pub(crate) fn load_companion_templates(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "テンプレート一覧の取得",
            workflows::list_companion_templates(host),
            Update::CompanionTemplates,
        );
    }

    pub(crate) fn export_companion(&mut self, companion_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        let target = companion_id.clone();
        self.request(
            "テンプレートの書き出し",
            workflows::export_companion(host, companion_id),
            move |json| Update::CompanionExported {
                companion_id: target,
                json,
            },
        );
        cx.notify();
    }

    pub(crate) fn import_companion_json(&mut self, raw: String, cx: &mut Context<Self>) {
        let template = match workflows::parse_template_json(&raw) {
            Ok(template) => template,
            Err(message) => {
                self.model
                    .notice(NoticeKind::Failed, "テンプレートの読み込み", message);
                cx.notify();
                return;
            }
        };
        self.import_companion_template(template, cx);
    }

    pub(crate) fn import_companion_template(
        &mut self,
        template: w::CompanionTemplate,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        let assignments = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(conn_id, model_id, _)| w::ModelAssignments {
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
            "テンプレートから作成",
            workflows::import_companion(host, template, assignments),
            |companion| Update::Created(companion.id),
        );
        cx.notify();
    }

    pub(crate) fn preview_companion(
        &mut self,
        name: String,
        mind: String,
        persona: String,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        self.model.preview_reply = None;
        let model = self
            .model_choices()
            .into_iter()
            .next()
            .map(|(connection_id, model_id, _)| w::ModelAssignment {
                connection_id,
                model_id,
                ..Default::default()
            });
        let request = w::PreviewCompanionRequest {
            name,
            mind,
            persona: (!persona.trim().is_empty()).then_some(persona),
            model: model.into(),
            user_message: "はじめまして！短く自己紹介してください。".into(),
            ..Default::default()
        };
        self.request(
            "コンパニオンのプレビュー",
            async move { Ok(workflows::preview_companion(host, request).await) },
            |update| update,
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
        self.load_mind_for(companion_id);
    }

    pub(crate) fn load_mind_for(&mut self, companion_id: String) {
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
        let always_approve = decision == w::ApprovalDecision::AlwaysApprove;
        self.request_with(
            "承認の送信",
            async move {
                let response = host
                    .settings
                    .submit_approval(w::SubmitApprovalRequest {
                        request_id: request,
                        decision: decision.into(),
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            move |reply| Update::Resolved {
                request_id: resolved,
                rule_warning: (always_approve && !reply.rule_saved).then(|| {
                    reply
                        .rule_error
                        .unwrap_or_else(|| "常に許可ルールを保存できませんでした。".into())
                }),
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
        let rule = w::PolicyRule {
            id: uuid::Uuid::new_v4().to_string(),
            tool_name,
            pattern,
            effect: effect.into(),
            ..Default::default()
        };
        self.update_policy_rules(
            "許可ルールの追加",
            w::UpdatePolicyRulesRequest {
                add_or_update: vec![rule],
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn remove_policy_rule(&mut self, id: String, cx: &mut Context<Self>) {
        self.update_policy_rules(
            "許可ルールの削除",
            w::UpdatePolicyRulesRequest {
                remove_ids: vec![id],
                ..Default::default()
            },
            cx,
        );
    }

    fn update_policy_rules(
        &mut self,
        context: &'static str,
        request: w::UpdatePolicyRulesRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            context,
            async move {
                let response = host.settings.update_policy_rules(request).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |rules| Update::PolicyRules(rules.items),
        );
        cx.notify();
    }

    pub(crate) fn load_policy_texts(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "自由文ポリシーの取得",
            async move {
                let response = host.settings.list_policy_texts(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |texts| Update::PolicyTexts(texts.items),
        );
    }

    pub(crate) fn load_plugins(&mut self, _cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プラグイン一覧の取得",
            async move {
                let response = host.settings.list_plugins(w::Empty::default()).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |list| Update::Plugins(list.plugins),
        );
    }

    pub(crate) fn install_plugin(&mut self, source_path: String, cx: &mut Context<Self>) {
        let trimmed = source_path.trim().to_owned();
        if trimmed.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プラグインの導入",
            async move {
                let _ = host
                    .settings
                    .install_plugin(w::InstallPluginRequest {
                        source_path: trimmed,
                        ..Default::default()
                    })
                    .await?;
                let list = host.settings.list_plugins(w::Empty::default()).await?;
                list.view().to_owned_message().map_err(decode)
            },
            |list| Update::Plugins(list.plugins),
        );
        cx.notify();
    }

    pub(crate) fn remote_host_config_path() -> Option<std::path::PathBuf> {
        directories::ProjectDirs::from("", "", "ene")
            .map(|dirs| dirs.config_dir().join("remote_host.json"))
    }

    pub(crate) fn current_remote_host() -> Option<String> {
        let path = Self::remote_host_config_path()?;
        match crate::host::HostTarget::read_remote(&path) {
            Ok(Some(crate::host::HostTarget::Remote { endpoint, .. })) => Some(endpoint),
            _ => None,
        }
    }

    pub(crate) fn save_remote_host(
        &mut self,
        endpoint: String,
        token: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(path) = Self::remote_host_config_path() else {
            return Err("設定ディレクトリを特定できませんでした。".into());
        };
        crate::host::HostTarget::write_remote(&path, endpoint.trim(), token.trim())
            .map_err(|error| error.to_string())?;
        self.model.notice(
            NoticeKind::Failed,
            "遠隔ホスト設定を保存しました",
            format!(
                "{} を保存しました。アプリ再起動時に遠隔 HTTPS ホストへ接続できます。",
                endpoint.trim()
            ),
        );
        cx.notify();
        Ok(())
    }

    pub(crate) fn clear_remote_host(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = Self::remote_host_config_path()
            && path.exists()
        {
            let _ = std::fs::remove_file(path);
        }
        cx.notify();
    }

    pub(crate) fn assign_search_connection(
        &mut self,
        connection_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs: Vec<w::ConnectionInput> = settings
            .connections
            .iter()
            .filter(|conn| conn.kind == w::ConnectionKind::Search)
            .map(|conn| w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name.clone(),
                enabled: connection_id.as_deref() == Some(conn.id.as_str()),
                expected_revision: conn.revision,
                kind: conn.kind,
                config: conn.config.as_option().cloned().into(),
                ..Default::default()
            })
            .collect();
        if inputs.is_empty() {
            return;
        }
        self.request(
            "Web 検索接続の割当",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            |updated| {
                Update::Notification(Box::new(w::Notification {
                    kind: "settings".into(),
                    event: Some(w::notification::Event::SettingsUpdated(Box::new(
                        w::SettingsUpdated {
                            settings: Some(updated).into(),
                            ..Default::default()
                        },
                    ))),
                    ..Default::default()
                }))
            },
        );
        cx.notify();
    }

    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn add_search_preset(
        &mut self,
        preset_id: &'static str,
        name: &'static str,
        adapter: &'static str,
        endpoint: &'static str,
        activate: bool,
        cx: &mut Context<Self>,
    ) -> String {
        let Some(settings) = self.model.settings().cloned() else {
            return preset_id.to_owned();
        };
        let Some(host) = self.host() else {
            return preset_id.to_owned();
        };
        let existing = settings.connections.iter().find(|conn| {
            conn.id == preset_id
                || search_adapter_of(conn)
                    .is_some_and(|existing_adapter| existing_adapter.eq_ignore_ascii_case(adapter))
        });
        let target_id = existing
            .map(|conn| conn.id.clone())
            .unwrap_or_else(|| preset_id.to_owned());
        let has_enabled_other = settings.connections.iter().any(|conn| {
            conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id
        });
        let should_enable = activate || !has_enabled_other || existing.is_some_and(|c| c.enabled);
        let mut inputs = Vec::new();
        if let Some(conn) = existing {
            inputs.push(w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name.clone(),
                enabled: should_enable,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: conn.config.as_option().cloned().into(),
                ..Default::default()
            });
        } else {
            inputs.push(search_connection_input(SearchConnectionSpec {
                id: target_id.clone(),
                name: name.to_owned(),
                adapter: adapter.to_owned(),
                endpoint: endpoint.to_owned(),
                enabled: should_enable,
                expected_revision: 0,
                max_results: 5,
            }));
        }
        if activate {
            for conn in &settings.connections {
                if conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id {
                    inputs.push(w::ConnectionInput {
                        id: conn.id.clone(),
                        name: conn.name.clone(),
                        enabled: false,
                        expected_revision: conn.revision,
                        kind: conn.kind,
                        config: conn.config.as_option().cloned().into(),
                        ..Default::default()
                    });
                }
            }
        }
        self.request(
            "Web 検索接続の追加",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            settings_update,
        );
        cx.notify();
        target_id
    }

    pub(crate) fn save_search_provider_key(
        &mut self,
        adapter: &'static str,
        secret: String,
        activate: bool,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() {
            return;
        }
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let (default_id, default_name, default_endpoint) = match adapter {
            "exa" => ("search-exa", "Web検索 (Exa)", "https://api.exa.ai/search"),
            "tavily" => (
                "search-tavily",
                "Web検索 (Tavily)",
                "https://api.tavily.com/search",
            ),
            "brave" => (
                "search-brave",
                "Web検索 (Brave)",
                "https://api.search.brave.com/res/v1/web/search",
            ),
            _ => return,
        };
        let existing = settings.connections.iter().find(|conn| {
            conn.id == default_id
                || search_adapter_of(conn)
                    .is_some_and(|existing_adapter| existing_adapter.eq_ignore_ascii_case(adapter))
        });
        let target_id = existing
            .map(|conn| conn.id.clone())
            .unwrap_or_else(|| default_id.to_owned());
        let has_enabled_other = settings.connections.iter().any(|conn| {
            conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id
        });
        let should_enable = activate || !has_enabled_other || existing.is_some_and(|c| c.enabled);
        let mut inputs = Vec::new();
        if let Some(conn) = existing {
            if conn.enabled != should_enable {
                inputs.push(w::ConnectionInput {
                    id: conn.id.clone(),
                    name: conn.name.clone(),
                    enabled: should_enable,
                    expected_revision: conn.revision,
                    kind: conn.kind,
                    config: conn.config.as_option().cloned().into(),
                    ..Default::default()
                });
            }
        } else {
            inputs.push(search_connection_input(SearchConnectionSpec {
                id: target_id.clone(),
                name: default_name.to_owned(),
                adapter: adapter.to_owned(),
                endpoint: default_endpoint.to_owned(),
                enabled: should_enable,
                expected_revision: 0,
                max_results: 5,
            }));
        }
        if activate {
            for conn in &settings.connections {
                if conn.kind == w::ConnectionKind::Search && conn.enabled && conn.id != target_id {
                    inputs.push(w::ConnectionInput {
                        id: conn.id.clone(),
                        name: conn.name.clone(),
                        enabled: false,
                        expected_revision: conn.revision,
                        kind: conn.kind,
                        config: conn.config.as_option().cloned().into(),
                        ..Default::default()
                    });
                }
            }
        }
        self.checking_connections.insert(target_id.clone());
        let updates = self.updates.clone();
        let check_id = target_id.clone();
        let return_id = target_id.clone();
        let settings_revision = settings.revision;
        self.request(
            "Web 検索APIキーの登録",
            async move {
                if !inputs.is_empty() {
                    let response = host
                        .settings
                        .update_settings(w::UpdateSettingsRequest {
                            expected_revision: settings_revision,
                            connections: inputs,
                            ..Default::default()
                        })
                        .await?;
                    let updated = response.view().to_owned_message().map_err(decode)?;
                    let _ = updates.send(settings_update(updated));
                }
                host.settings
                    .register_credentials(w::CredentialRequest {
                        id: target_id.clone(),
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await?;
                let check_host = host.clone();
                let check_updates = updates.clone();
                tokio::spawn(async move {
                    let update = workflows::probe_connection(check_host, check_id).await;
                    let _ = check_updates.send(update);
                });
                Ok::<_, connectrpc::ConnectError>(())
            },
            move |_| Update::CredentialRegistered {
                connection_id: return_id,
            },
        );
        cx.notify();
    }

    pub(crate) fn set_plugin_enabled(
        &mut self,
        plugin_id: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プラグイン状態の更新",
            async move {
                let _ = host
                    .settings
                    .set_plugin_enabled(w::SetPluginEnabledRequest {
                        plugin_id,
                        enabled,
                        ..Default::default()
                    })
                    .await?;
                let list = host.settings.list_plugins(w::Empty::default()).await?;
                list.view().to_owned_message().map_err(decode)
            },
            |list| Update::Plugins(list.plugins),
        );
        cx.notify();
    }

    pub(crate) fn remove_plugin(&mut self, plugin_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "プラグインの削除",
            async move {
                let _ = host
                    .settings
                    .remove_plugin(w::RemovePluginRequest {
                        plugin_id,
                        ..Default::default()
                    })
                    .await?;
                let list = host.settings.list_plugins(w::Empty::default()).await?;
                list.view().to_owned_message().map_err(decode)
            },
            |list| Update::Plugins(list.plugins),
        );
        cx.notify();
    }

    pub(crate) fn upsert_policy_text(
        &mut self,
        id: Option<String>,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let entry = w::PolicyText {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            text,
            ..Default::default()
        };
        self.update_policy_texts(
            "自由文ポリシーの保存",
            w::UpdatePolicyTextsRequest {
                add_or_update: vec![entry],
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn remove_policy_text(&mut self, id: String, cx: &mut Context<Self>) {
        self.update_policy_texts(
            "自由文ポリシーの削除",
            w::UpdatePolicyTextsRequest {
                remove_ids: vec![id],
                ..Default::default()
            },
            cx,
        );
    }

    fn update_policy_texts(
        &mut self,
        context: &'static str,
        request: w::UpdatePolicyTextsRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            context,
            async move {
                let response = host.settings.update_policy_texts(request).await?;
                response.view().to_owned_message().map_err(decode)
            },
            |texts| Update::PolicyTexts(texts.items),
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
            settings_update,
        );
        cx.notify();
    }

    pub(crate) fn delete_connection(&mut self, id: String, revision: u64, cx: &mut Context<Self>) {
        let Some(host) = self.host() else { return };
        self.request(
            "接続の削除",
            async move {
                let response = host
                    .settings
                    .delete_connection(w::DeleteConnectionRequest {
                        id,
                        expected_revision: revision,
                        ..Default::default()
                    })
                    .await?;
                response.view().to_owned_message().map_err(decode)
            },
            settings_update,
        );
        cx.notify();
    }

    pub(crate) fn set_connection_enabled(
        &mut self,
        connection: w::Connection,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let is_voicevox = is_local_voicevox_conn(&connection);
        let conn_id = connection.id.clone();
        let input = w::ConnectionInput {
            id: connection.id,
            name: connection.name,
            enabled,
            expected_revision: connection.revision,
            kind: connection.kind,
            config: connection.config,
            ..Default::default()
        };
        if enabled && is_voicevox {
            self.checking_connections.insert(conn_id.clone());
            let updates = self.updates.clone();
            let runtime_host = host.clone();
            self.request(
                "接続状態の更新",
                async move {
                    let response = host
                        .settings
                        .update_settings(w::UpdateSettingsRequest {
                            expected_revision: settings.revision,
                            connections: vec![input],
                            ..Default::default()
                        })
                        .await?;
                    let updated = response.view().to_owned_message().map_err(decode)?;
                    let check_id = conn_id.clone();
                    let check_host = runtime_host.clone();
                    let check_updates = updates.clone();
                    tokio::spawn(async move {
                        let check_res = check_host
                            .settings
                            .check_connection(w::CheckRequest {
                                id: check_id.clone(),
                                request_key: uuid::Uuid::new_v4().to_string(),
                                ..Default::default()
                            })
                            .await;
                        let update = match check_res {
                            Ok(resp) => match resp.view().to_owned_message() {
                                Ok(res) => Update::Checked {
                                    connection_id: check_id,
                                    check: res.check.into_option().unwrap_or_default(),
                                },
                                Err(err) => failure("接続確認", decode(err)),
                            },
                            Err(err) => failure("接続確認", err),
                        };
                        let _ = check_updates.send(update);
                    });
                    Ok(updated)
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
        } else {
            self.upsert_connection("接続状態の更新", input, cx);
        }
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

    pub(crate) fn quick_connect_llm(
        &mut self,
        provider: w::LlmProvider,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings() else {
            return;
        };
        let Some(host) = self.host() else { return };
        if self.setup_pending {
            return;
        }
        // Reuse a failed attempt only at the exact catalog endpoint.
        let existing = settings.connections.iter().find(|c| {
            c.enabled
                && llm_endpoint(c).is_some_and(|ep| {
                    ep.adapter == provider.adapter
                        && ep.endpoint == provider.endpoint
                        && (ep.adapter != GOOGLE_AGENT_PLATFORM
                            || serde_json::from_str::<serde_json::Value>(&ep.options)
                                .ok()
                                .and_then(|options| {
                                    options["project_id"]
                                        .as_str()
                                        .map(|id| !id.trim().is_empty())
                                })
                                != Some(true))
                })
        });
        let id = existing
            .map(|c| c.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let revision = settings.revision;
        let create = existing.is_none();
        let input = llm_provider_input(id.clone(), provider, Vec::new());
        self.setup_pending = true;
        self.setup_error = None;
        self.setup_error_detail = None;
        self.request(
            "AI接続",
            async move {
                Ok(workflows::quick_connect_llm(host, revision, id, create, input, secret).await)
            },
            |outcome| outcome,
        );
        cx.notify();
    }

    pub(crate) fn complete_setup_selection(
        &mut self,
        connection_id: String,
        model_id: String,
        companion_name: Option<String>,
        voice_identifier: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let model_id = model_id.trim().to_owned();
        if model_id.is_empty() {
            return;
        }
        let conn = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned();
        let existing_comp = self.model.companion().cloned();
        let voice_clean = voice_identifier
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty());
        let name_clean = companion_name
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "ene".to_owned());

        self.pending_setup_connection_id = None;
        self.pending_setup_models.clear();

        self.request(
            "初期設定の完了",
            workflows::complete_setup_selection(
                host,
                workflows::SetupSelection {
                    settings_revision: settings.revision,
                    conn,
                    connection_id,
                    model_id,
                    existing_comp,
                    companion_name: name_clean,
                    voice_identifier: voice_clean,
                },
            ),
            |update| update,
        );
        cx.notify();
    }

    pub(crate) fn add_voicevox_preset(
        &mut self,
        speaker: u32,
        speed: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let existing_rev = settings
            .connections
            .iter()
            .find(|c| c.id == "voicevox")
            .map(|c| c.revision)
            .unwrap_or(0);
        let options = match speed {
            Some(speed) => format!("{{\"speaker\":{speaker},\"speed\":{speed:.2}}}"),
            None => format!("{{\"speaker\":{speaker}}}"),
        };
        let input = w::ConnectionInput {
            id: "voicevox".into(),
            name: "VOICEVOX（ローカル）".into(),
            enabled: true,
            expected_revision: existing_rev,
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
        };
        self.checking_connections.insert("voicevox".into());
        let updates = self.updates.clone();
        let probe_host = host.clone();
        self.request(
            "VOICEVOX接続の追加",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: vec![input],
                        ..Default::default()
                    })
                    .await?;
                let updated = response.view().to_owned_message().map_err(decode)?;
                tokio::spawn(async move {
                    let check_res = probe_host
                        .settings
                        .check_connection(w::CheckRequest {
                            id: "voicevox".into(),
                            request_key: uuid::Uuid::new_v4().to_string(),
                            ..Default::default()
                        })
                        .await;
                    let update = match check_res {
                        Ok(resp) => match resp.view().to_owned_message() {
                            Ok(res) => Update::Checked {
                                connection_id: "voicevox".into(),
                                check: res.check.into_option().unwrap_or_default(),
                            },
                            Err(err) => failure("接続確認", decode(err)),
                        },
                        Err(err) => failure("接続確認", err),
                    };
                    let _ = updates.send(update);
                });
                Ok(updated)
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
        self.add_stt_preset_with_spec(
            SttPreset {
                fixed_id: None,
                adapter: "openai",
                name,
                endpoint,
                model,
                language,
            },
            cx,
        );
    }

    fn add_stt_preset_with_spec(&mut self, spec: SttPreset<'_>, cx: &mut Context<Self>) {
        let existing_rev = spec
            .fixed_id
            .and_then(|fid| {
                self.model.settings().and_then(|s| {
                    s.connections
                        .iter()
                        .find(|c| c.id == fid)
                        .map(|c| c.revision)
                })
            })
            .unwrap_or(0);
        let protocol = if spec.adapter == "sakura" {
            "sakura"
        } else {
            "openai"
        };
        let options = serde_json::json!({
            "protocol": protocol,
            "model": if spec.model.trim().is_empty() { "whisper-1" } else { spec.model.trim() },
            "language": if spec.language.trim().is_empty() { "ja" } else { spec.language.trim() },
        })
        .to_string();
        let lang = if spec.language.trim().is_empty() {
            "ja".to_owned()
        } else {
            spec.language.trim().to_owned()
        };
        self.upsert_connection(
            "音声認識（STT）接続の追加",
            w::ConnectionInput {
                id: spec
                    .fixed_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: spec.name,
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Stt.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Stt(Box::new(
                        w::SttConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: spec.adapter.to_owned(),
                                endpoint: spec.endpoint,
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

    pub(crate) fn add_sakura_tts_preset(&mut self, cx: &mut Context<Self>) {
        let existing_rev = self
            .model
            .settings()
            .and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == "sakura-tts")
                    .map(|c| c.revision)
            })
            .unwrap_or(0);
        let options = serde_json::json!({
            "protocol": "sakura",
            "model": "zundamon",
            "voice": "normal",
            "response_format": "wav",
        })
        .to_string();
        self.upsert_connection(
            "さくらのAI Engine TTS接続の追加",
            w::ConnectionInput {
                id: "sakura-tts".into(),
                name: "さくらのAI Engine TTS".into(),
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Tts.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Tts(Box::new(
                        w::TtsConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: "sakura".into(),
                                endpoint: SAKURA_ENDPOINT.into(),
                                options_schema: String::new(),
                                options,
                                ..Default::default()
                            })
                            .into(),
                            voice_ids: vec!["zundamon/normal".into()],
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

    pub(crate) fn add_sakura_stt_preset(&mut self, cx: &mut Context<Self>) {
        self.add_stt_preset_with_spec(
            SttPreset {
                fixed_id: Some("sakura-stt"),
                adapter: "sakura",
                name: "さくらのAI Engine STT".into(),
                endpoint: SAKURA_ENDPOINT.into(),
                model: "whisper-large-v3-turbo".into(),
                language: "ja".into(),
            },
            cx,
        );
    }

    pub(crate) fn add_sakura_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            "sakura-embedding",
            "さくらのAI Engine Embedding",
            "sakura",
            SAKURA_ENDPOINT,
            "multilingual-e5-large",
            1024,
            cx,
        );
    }

    pub(crate) fn add_openai_embedding_preset(&mut self, cx: &mut Context<Self>) {
        self.add_embedding_preset(
            "openai-embedding",
            "OpenAI Embedding",
            "openai",
            "https://api.openai.com/v1",
            "text-embedding-3-small",
            1536,
            cx,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn add_embedding_preset(
        &mut self,
        id: &str,
        name: &str,
        adapter: &str,
        endpoint: &str,
        model_id: &str,
        dimensions: u32,
        cx: &mut Context<Self>,
    ) {
        let existing_rev = self
            .model
            .settings()
            .and_then(|s| {
                s.connections
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.revision)
            })
            .unwrap_or(0);
        let should_assign = self.model.companion().is_some_and(|c| {
            c.assignments
                .as_option()
                .and_then(|a| a.embedding.as_option())
                .is_none_or(|e| e.connection_id.trim().is_empty())
        });
        self.upsert_connection(
            "埋め込み（Embedding）接続の追加",
            w::ConnectionInput {
                id: id.into(),
                name: name.into(),
                enabled: true,
                expected_revision: existing_rev,
                kind: w::ConnectionKind::Embedding.into(),
                config: Some(w::ConnectionConfig {
                    config: Some(w::connection_config::Config::Embedding(Box::new(
                        w::EmbeddingConnection {
                            endpoint: Some(w::ConnectionEndpoint {
                                adapter: adapter.into(),
                                endpoint: endpoint.into(),
                                options_schema: String::new(),
                                options: "{}".into(),
                                ..Default::default()
                            })
                            .into(),
                            model_id: model_id.into(),
                            dimensions,
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
        if should_assign {
            self.assign_model(
                ModelRoleSlot::Embedding,
                Some((id.into(), model_id.into())),
                cx,
            );
        }
    }

    pub(crate) fn check_connection(&mut self, connection_id: String, cx: &mut Context<Self>) {
        let Some(host) = self.host() else {
            return;
        };
        if self.checking_connections.contains(&connection_id) {
            return;
        }
        self.checking_connections.insert(connection_id.clone());
        self.request(
            "接続確認",
            async move { Ok(workflows::probe_connection(host, connection_id).await) },
            |update| update,
        );
        cx.notify();
    }

    pub(crate) fn select_connection_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(conn) = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
        else {
            return;
        };
        let model_id = model_id.trim().to_owned();
        if model_id.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        if let Some(w::connection_config::Config::Llm(llm)) = cfg.config.as_mut() {
            llm.model_ids = vec![model_id.clone()];
        }
        self.upsert_connection(
            "使用モデルの変更",
            w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
        if let Some(comp) = self.model.companion().cloned() {
            let assignments = comp.assignments.as_option();
            let comp_uses_conn = assignments
                .and_then(|a| a.companion.as_option())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            let task_uses_conn = assignments
                .and_then(|a| a.task.as_option())
                .is_none_or(|a| a.connection_id.is_empty() || a.connection_id == connection_id);
            if comp_uses_conn {
                self.assign_model(
                    ModelRoleSlot::Companion,
                    Some((connection_id.clone(), model_id.clone())),
                    cx,
                );
            }
            if task_uses_conn {
                self.assign_model(ModelRoleSlot::Task, Some((connection_id, model_id)), cx);
            }
        }
    }

    pub(crate) fn select_voicevox_speaker(
        &mut self,
        connection_id: String,
        speaker_id: String,
        cx: &mut Context<Self>,
    ) {
        self.select_tts_voice(connection_id, speaker_id, cx);
    }

    pub(crate) fn select_tts_voice(
        &mut self,
        connection_id: String,
        voice_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(conn) = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
        else {
            return;
        };
        let voice_clean = voice_id.trim().to_owned();
        if voice_clean.is_empty() {
            return;
        }
        let is_sakura = is_sakura_conn(&conn);
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        apply_tts_voice_to_config(&mut cfg, is_sakura, &voice_clean);
        self.upsert_connection(
            "音声話者の変更",
            w::ConnectionInput {
                id: conn.id,
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
    }

    pub(crate) fn select_stt_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(conn) = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
        else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        apply_stt_model_to_config(&mut cfg, &model_clean);
        self.upsert_connection(
            "音声認識モデルの変更",
            w::ConnectionInput {
                id: conn.id,
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
    }

    /// Assigns the active STT connection and model atomically across all STT connections.
    pub(crate) fn assign_stt_model(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs = build_stt_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, mid)| (cid.as_str(), mid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        self.request(
            "音声認識モデルの割当",
            async move {
                let response = host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
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

    /// Assigns the active TTS connection and voice atomically across all TTS connections,
    /// probing local VOICEVOX when newly enabled and syncing companion `voice_identifier`.
    pub(crate) fn assign_tts_voice(
        &mut self,
        selection: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(host) = self.host() else {
            return;
        };
        let inputs = build_tts_assignment_inputs(
            &settings.connections,
            selection
                .as_ref()
                .map(|(cid, vid)| (cid.as_str(), vid.as_str())),
        );
        if inputs.is_empty() {
            return;
        }
        let target_voice_clean = selection
            .as_ref()
            .map(|(_, vid)| vid.trim().to_owned())
            .filter(|v| !v.is_empty());
        let probe_voicevox_id = selection.as_ref().and_then(|(cid, _)| {
            let conn = settings.connections.iter().find(|c| c.id == *cid)?;
            if is_local_voicevox_conn(conn)
                && (!conn.enabled || self.voice_choices_for_connection(&conn.id).len() <= 1)
            {
                Some(conn.id.clone())
            } else {
                None
            }
        });
        if let Some(ref check_id) = probe_voicevox_id {
            self.checking_connections.insert(check_id.clone());
        }
        let check_updates = self.updates.clone();
        let probe_host = host.clone();
        let settings_host = host.clone();
        self.request(
            "音声合成（TTS）の割当",
            async move {
                let response = settings_host
                    .settings
                    .update_settings(w::UpdateSettingsRequest {
                        expected_revision: settings.revision,
                        connections: inputs,
                        ..Default::default()
                    })
                    .await?;
                let updated = response.view().to_owned_message().map_err(decode)?;
                if let Some(check_id) = probe_voicevox_id {
                    tokio::spawn(async move {
                        let check_res = probe_host
                            .settings
                            .check_connection(w::CheckRequest {
                                id: check_id.clone(),
                                request_key: uuid::Uuid::new_v4().to_string(),
                                ..Default::default()
                            })
                            .await;
                        let update = match check_res {
                            Ok(resp) => match resp.view().to_owned_message() {
                                Ok(res) => Update::Checked {
                                    connection_id: check_id,
                                    check: res.check.into_option().unwrap_or_default(),
                                },
                                Err(err) => failure("接続確認", decode(err)),
                            },
                            Err(err) => failure("接続確認", err),
                        };
                        let _ = check_updates.send(update);
                    });
                }
                Ok(updated)
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
        if let Some(voice_clean) = target_voice_clean
            && let Some(comp) = self.model.companion().cloned()
            && comp.voice_identifier.as_deref() != Some(voice_clean.as_str())
        {
            self.request(
                "コンパニオン話者の同期",
                async move {
                    let response = host
                        .host
                        .update_companion(w::UpdateCompanionRequest {
                            id: comp.id,
                            revision: comp.revision,
                            voice_identifier: Some(voice_clean),
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
        }
        cx.notify();
    }

    pub(crate) fn select_embedding_model(
        &mut self,
        connection_id: String,
        model_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(settings) = self.model.settings().cloned() else {
            return;
        };
        let Some(conn) = settings
            .connections
            .iter()
            .find(|c| c.id == connection_id)
            .cloned()
        else {
            return;
        };
        let model_clean = model_id.trim().to_owned();
        if model_clean.is_empty() {
            return;
        }
        let mut cfg = conn.config.as_option().cloned().unwrap_or_default();
        if let Some(w::connection_config::Config::Embedding(emb)) = cfg.config.as_mut() {
            emb.model_id = model_clean.clone();
            emb.dimensions = default_embedding_dimensions(&model_clean);
        }
        self.upsert_connection(
            "埋め込みモデルの変更",
            w::ConnectionInput {
                id: conn.id.clone(),
                name: conn.name,
                enabled: conn.enabled,
                expected_revision: conn.revision,
                kind: conn.kind,
                config: Some(cfg).into(),
                ..Default::default()
            },
            cx,
        );
        if let Some(comp) = self.model.companion()
            && comp
                .assignments
                .as_option()
                .and_then(|a| a.embedding.as_option())
                .is_some_and(|a| a.connection_id == connection_id)
        {
            self.assign_model(
                ModelRoleSlot::Embedding,
                Some((connection_id, model_clean)),
                cx,
            );
        }
    }

    pub(crate) fn register_secret(
        &mut self,
        connection_id: String,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        let is_search = self.model.settings().is_some_and(|s| {
            s.connections
                .iter()
                .any(|c| c.id == connection_id && c.kind == w::ConnectionKind::Search)
        });
        if is_search {
            self.checking_connections.insert(connection_id.clone());
        }
        let id = connection_id.clone();
        let updates = self.updates.clone();
        self.request(
            "資格情報の登録",
            async move {
                let response = host
                    .settings
                    .register_credentials(w::CredentialRequest {
                        id: connection_id.clone(),
                        secret: Some(secret),
                        ..Default::default()
                    })
                    .await?;
                let out = response.view().to_owned_message().map_err(decode)?;
                if is_search {
                    let check_host = host.clone();
                    let check_id = connection_id;
                    let check_updates = updates.clone();
                    tokio::spawn(async move {
                        let update = workflows::probe_connection(check_host, check_id).await;
                        let _ = check_updates.send(update);
                    });
                }
                Ok(out)
            },
            move |_| Update::CredentialRegistered { connection_id: id },
        );
        cx.notify();
    }

    pub(crate) fn register_secret_for_connections(
        &mut self,
        connection_ids: Vec<String>,
        secret: String,
        cx: &mut Context<Self>,
    ) {
        let secret = secret.trim().to_owned();
        if secret.is_empty() || connection_ids.is_empty() {
            return;
        }
        let Some(host) = self.host() else {
            return;
        };
        self.request(
            "複数接続への資格情報登録",
            async move {
                for id in connection_ids {
                    host.settings
                        .register_credentials(w::CredentialRequest {
                            id,
                            secret: Some(secret.clone()),
                            ..Default::default()
                        })
                        .await?;
                }
                Ok::<_, connectrpc::ConnectError>(())
            },
            |_| Update::Resnapshot,
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

    /// Discovered TTS voice choices `(connection_id, voice_id, display_name, connection_name)`
    /// from enabled TTS connections, keeping duplicate `voice_id`s across connections distinct.
    pub(crate) fn tts_voice_choices(&self) -> Vec<(String, String, String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let mut voices: Vec<(String, String, String, String)> = Vec::new();
        for conn in &settings.connections {
            if conn.kind != w::ConnectionKind::Tts || !conn.enabled {
                continue;
            }
            for (vid, label) in self.voice_choices_for_connection(&conn.id) {
                voices.push((conn.id.clone(), vid, label, conn.name.clone()));
            }
        }
        voices
    }

    /// Discovered `(voice_id, display_name)` choices for a single TTS connection.
    pub(crate) fn voice_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let Some(conn) = settings.connections.iter().find(|c| c.id == conn_id) else {
            return Vec::new();
        };
        let mut voices: Vec<(String, String)> = Vec::new();
        fn push_voice_unique(voices: &mut Vec<(String, String)>, raw: &str) {
            let (vid, label) = parse_voice_resource(raw);
            if vid.is_empty() {
                return;
            }
            if let Some(existing) = voices.iter_mut().find(|(id, _)| id == &vid) {
                if existing.1 == existing.0 && label != vid {
                    existing.1 = label;
                }
            } else {
                voices.push((vid, label));
            }
        }
        if let Some(check) = self
            .model
            .checks
            .get(&conn.id)
            .or_else(|| conn.last_check.as_option())
        {
            for res in &check.resources {
                push_voice_unique(&mut voices, res);
            }
        }
        let sakura = is_sakura_conn(conn);
        if let Some(w::connection_config::Config::Tts(tts)) =
            conn.config.as_option().and_then(|c| c.config.as_ref())
        {
            for id in &tts.voice_ids {
                push_voice_unique(&mut voices, id);
            }
            if !sakura
                && tts
                    .endpoint
                    .as_option()
                    .is_some_and(|ep| ep.adapter.trim().eq_ignore_ascii_case("openai"))
            {
                for voice in OPENAI_TTS_VOICES {
                    push_voice_unique(&mut voices, voice);
                }
            }
        }
        if sakura {
            for entry in SAKURA_TTS_VOICES {
                push_voice_unique(&mut voices, entry);
            }
        } else if voices.is_empty()
            && let Some(configured) = tts_voice_of_connection(conn)
        {
            push_voice_unique(&mut voices, &configured);
        }
        voices
    }

    pub(crate) fn stt_model_choices_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let Some(conn) = settings.connections.iter().find(|c| c.id == conn_id) else {
            return Vec::new();
        };
        let mut models: Vec<(String, String)> = Vec::new();
        fn push_str_unique(list: &mut Vec<(String, String)>, id: &str) {
            let trimmed = id.trim();
            if !trimmed.is_empty() && !list.iter().any(|(existing, _)| existing == trimmed) {
                list.push((trimmed.to_owned(), trimmed.to_owned()));
            }
        }
        if let Some(w::connection_config::Config::Stt(stt)) =
            conn.config.as_option().and_then(|c| c.config.as_ref())
            && let Some(ep) = stt.endpoint.as_option()
            && let Ok(serde_json::Value::Object(opts)) = serde_json::from_str(&ep.options)
            && let Some(model) = opts.get("model").and_then(|v| v.as_str())
        {
            push_str_unique(&mut models, model);
        }
        let sakura = is_sakura_conn(conn);
        if let Some(check) = self
            .model
            .checks
            .get(&conn.id)
            .or_else(|| conn.last_check.as_option())
        {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if !sakura || lower.contains("whisper") || lower.contains("transcribe") {
                    push_str_unique(&mut models, res);
                }
            }
        }
        let defaults: &[&str] = if sakura {
            SAKURA_STT_MODELS
        } else {
            &["gpt-4o-mini-transcribe", "gpt-4o-transcribe", "whisper-1"]
        };
        for id in defaults {
            push_str_unique(&mut models, id);
        }
        models
    }

    pub(crate) fn embedding_model_choices_for_connection(
        &self,
        conn_id: &str,
    ) -> Vec<(String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let Some(conn) = settings.connections.iter().find(|c| c.id == conn_id) else {
            return Vec::new();
        };
        let mut models: Vec<(String, String)> = Vec::new();
        fn push_emb_unique(list: &mut Vec<(String, String)>, id: &str) {
            let trimmed = id.trim();
            if !trimmed.is_empty() && !list.iter().any(|(existing, _)| existing == trimmed) {
                list.push((trimmed.to_owned(), trimmed.to_owned()));
            }
        }
        if let Some(w::connection_config::Config::Embedding(emb)) =
            conn.config.as_option().and_then(|c| c.config.as_ref())
        {
            push_emb_unique(&mut models, &emb.model_id);
        }
        if let Some(check) = self
            .model
            .checks
            .get(&conn.id)
            .or_else(|| conn.last_check.as_option())
        {
            for res in &check.resources {
                let lower = res.to_ascii_lowercase();
                if lower.contains("embed") || lower.contains("e5") {
                    push_emb_unique(&mut models, res);
                }
            }
        }
        let defaults: &[&str] = if is_sakura_conn(conn) {
            SAKURA_EMBEDDING_MODELS
        } else {
            &["text-embedding-3-small", "text-embedding-3-large"]
        };
        for id in defaults {
            push_emb_unique(&mut models, id);
        }
        models
    }

    /// Available `(model_id, display_label)` choices for a single LLM connection,
    /// merging configured `model_ids`, fetched `ListModels`, and `ConnectionCheck.resources`.
    pub(crate) fn available_models_for_connection(&self, conn_id: &str) -> Vec<(String, String)> {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let Some(connection) = settings.connections.iter().find(|c| c.id == conn_id) else {
            return Vec::new();
        };
        let (configured, adapter) = match connection
            .config
            .as_option()
            .and_then(|config| config.config.as_ref())
        {
            Some(w::connection_config::Config::Llm(llm)) => (
                llm.model_ids.clone(),
                llm.endpoint
                    .as_option()
                    .map(|ep| ep.adapter.as_str())
                    .unwrap_or_default(),
            ),
            _ => (Vec::new(), ""),
        };
        let fetched =
            if adapter == GOOGLE_AGENT_PLATFORM && connection.last_check.as_option().is_none() {
                Vec::new()
            } else {
                self.model
                    .models
                    .get(&connection.id)
                    .cloned()
                    .unwrap_or_default()
            };
        if adapter == GOOGLE_AGENT_PLATFORM
            && let Some(check) = connection.last_check.as_option()
            && check
                .capabilities
                .iter()
                .any(|cap| cap == "verified_model_list")
        {
            return check
                .resources
                .iter()
                .map(|id| {
                    let label = fetched
                        .iter()
                        .find(|item| &item.id == id)
                        .filter(|item| !item.name.trim().is_empty())
                        .map_or(id, |item| &item.name);
                    (id.clone(), label.clone())
                })
                .collect();
        }
        let checked = if adapter == GOOGLE_AGENT_PLATFORM {
            connection
                .last_check
                .as_option()
                .map(|check| check.resources.clone())
                .unwrap_or_default()
        } else {
            self.model
                .checks
                .get(&connection.id)
                .or_else(|| connection.last_check.as_option())
                .map(|check| check.resources.clone())
                .unwrap_or_default()
        };

        let mut list: Vec<(String, String)> = Vec::new();
        fn push_unique_model(list: &mut Vec<(String, String)>, id: &str, name: Option<&str>) {
            let id = id.trim();
            if id.is_empty() {
                return;
            }
            let display = name.map(str::trim).filter(|n| !n.is_empty()).unwrap_or(id);
            if let Some(existing) = list.iter_mut().find(|(eid, _)| eid == id) {
                if existing.1 == existing.0 && display != id {
                    existing.1 = display.to_owned();
                }
            } else {
                list.push((id.to_owned(), display.to_owned()));
            }
        }

        for id in &configured {
            let fetched_name = fetched
                .iter()
                .find(|m| m.id == *id)
                .map(|m| m.name.as_str());
            push_unique_model(&mut list, id, fetched_name);
        }
        for model in &fetched {
            push_unique_model(&mut list, &model.id, Some(&model.name));
        }
        for id in &checked {
            push_unique_model(&mut list, id, None);
        }
        if list.is_empty() {
            for id in default_models_for_adapter(adapter) {
                push_unique_model(&mut list, id, None);
            }
        }
        list
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
            for (model_id, label) in self.available_models_for_connection(&connection.id) {
                choices.push((
                    connection.id.clone(),
                    model_id,
                    format!("{} / {label}", connection.name),
                ));
            }
        }
        choices
    }

    fn collect_connection_choices<F>(
        &self,
        kind: w::ConnectionKind,
        choices_for: F,
    ) -> Vec<(String, String, String)>
    where
        F: Fn(&Self, &str) -> Vec<(String, String)>,
    {
        let Some(settings) = self.model.settings() else {
            return Vec::new();
        };
        let mut choices = Vec::new();
        for connection in &settings.connections {
            if connection.kind != kind {
                continue;
            }
            for (id, label) in choices_for(self, &connection.id) {
                choices.push((
                    connection.id.clone(),
                    id,
                    format!("{} / {label}", connection.name),
                ));
            }
        }
        choices
    }

    /// Model choices across all LLM connections for progressive-disclosure slot overrides.
    pub(crate) fn all_llm_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(
            w::ConnectionKind::Llm,
            Self::available_models_for_connection,
        )
    }

    /// Model choices across embedding connections for the embedding role slot.
    pub(crate) fn all_embedding_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(
            w::ConnectionKind::Embedding,
            Self::embedding_model_choices_for_connection,
        )
    }

    /// Model choices across all STT connections (`enabled` or disabled).
    pub(crate) fn all_stt_models(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(
            w::ConnectionKind::Stt,
            Self::stt_model_choices_for_connection,
        )
    }

    /// Currently active `(connection_id, model_id)` for STT (`None` if all STT connections are off).
    pub(crate) fn active_stt_selection(&self) -> Option<(String, String)> {
        let settings = self.model.settings()?;
        active_stt_in_connections(&settings.connections)
    }

    /// Voice choices across all TTS connections (`enabled` or disabled).
    pub(crate) fn all_tts_voices(&self) -> Vec<(String, String, String)> {
        self.collect_connection_choices(w::ConnectionKind::Tts, Self::voice_choices_for_connection)
    }

    /// Currently active `(connection_id, voice_id)` for TTS (`None` if all TTS connections are off).
    pub(crate) fn active_tts_selection(&self) -> Option<(String, String)> {
        let settings = self.model.settings()?;
        active_tts_in_connections(&settings.connections)
    }
}

fn settings_window_bounds(cx: &gpui::App) -> Bounds<gpui::Pixels> {
    let default_size = size(px(820.), px(720.));
    if let Some(display) = cx.primary_display() {
        let visible = display.bounds();
        let width = px(820.).min((visible.size.width - px(96.)).max(px(680.)));
        let height = px(740.).min((visible.size.height - px(96.)).max(px(480.)));
        // Place the settings window on the left half when wide enough so it
        // does not collide with the centered desktop avatar.
        if visible.size.width >= px(1240.) {
            return Bounds::new(
                gpui::point(visible.origin.x + px(48.), visible.origin.y + px(44.)),
                size(width, height),
            );
        }
        return Bounds::centered(Some(display.id()), size(width, height), cx);
    }
    Bounds::centered(None, default_size, cx)
}

struct SttPreset<'a> {
    fixed_id: Option<&'a str>,
    adapter: &'a str,
    name: String,
    endpoint: String,
    model: String,
    language: String,
}

/// Opens the floating companion pod window and registers it with `Hub`.
pub fn open_pod_window(cx: &mut App, hub: Entity<Hub>) -> Option<WindowHandle<CompanionPod>> {
    let mode = hub.read(cx).pod_mode;
    let bounds = initial_pod_bounds_for_mode(mode, cx);
    let hub_for_pod = hub.clone();
    let opened = cx.open_window(
        WindowOptions {
            #[cfg(windows)]
            kind: WindowKind::PopUp,
            #[cfg(not(windows))]
            kind: WindowKind::Floating,
            window_background: gpui::WindowBackgroundAppearance::Transparent,
            window_decorations: Some(gpui::WindowDecorations::Client),
            app_id: Some("ene-desktop".into()),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(PodMode::MiniVoice.min_size()),
            is_movable: true,
            is_resizable: false,
            titlebar: None,
            ..Default::default()
        },
        move |_window, cx| cx.new(|cx| CompanionPod::new(hub_for_pod.clone(), cx)),
    );
    match opened {
        Ok(window) => {
            let hub_close = hub.clone();
            if let Ok(view) = window.update(cx, |view, window, cx| {
                focus_chat(view, window, cx);
                window.on_window_should_close(cx, move |_window, cx| {
                    let should_hide = hub_close.read(cx).is_tray_resident()
                        || hub_close.read(cx).settings_window.is_some();
                    if should_hide {
                        hub_close.update(cx, |hub, cx| hub.hide_pod(cx));
                        false
                    } else {
                        true
                    }
                });
                cx.entity().downgrade()
            }) {
                hub.update(cx, |hub, cx| hub.register_pod_window(window, view, cx));
            }
            Some(window)
        }
        Err(error) => {
            eprintln!("ene-desktop: the window did not open: {error:#}");
            None
        }
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

fn settings_update(settings: w::Settings) -> Update {
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
}

/// Keeps a per-window draft entity map synced with the active field theme.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::too_many_arguments)]
pub(crate) fn window_draft(
    drafts: &mut BTreeMap<String, gpui::Entity<crate::input::Draft>>,
    cx: &mut gpui::App,
    key: impl Into<String>,
    placeholder: &'static str,
    initial: Option<&str>,
    look: crate::input::FieldLook,
    context: &'static str,
) -> gpui::Entity<crate::input::Draft> {
    let entity = drafts
        .entry(key.into())
        .or_insert_with(|| {
            cx.new(|cx| {
                let mut draft = crate::input::Draft::new(cx, placeholder, context);
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
