//! What the window shows, and how host messages become that view.
//!
//! The host is the authority for every piece of shared state. This module keeps
//! exactly one display copy of each: the last `Snapshot` for companions,
//! settings, clients, approvals, policy rules and activations, and the
//! selected companion's history. Notifications are merged
//! into those copies, never into a second parallel model, and nothing here is
//! persisted.
//!
//! Two rules matter for correctness:
//!
//! - `MessageDelta` text is a preview keyed by the host's stream id. It becomes
//!   a message only when the host records one, which always arrives as
//!   `MessageCreated`.
//! - A revision the host has already moved past never replaces a newer one, so
//!   a replayed or reordered notification cannot roll the window back.

use crate::update::Update;
use ene_rpc as w;
use std::{collections::BTreeMap, time::Duration};

/// The link to the host, in the states the window must tell apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// No discovery file yet: the host has not started.
    Unset { path: String, reason: String },
    /// The discovery file was read; registration and subscription are opening.
    Connecting { endpoint: String, attempt: u32 },
    /// Subscribed and receiving notifications.
    Live { version: String },
    /// The link dropped; the session retries after `retry_in`.
    Lost { reason: String, retry_in: Duration },
    /// Retrying cannot help until the configuration changes.
    Unusable { reason: String },
}

impl Link {
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live { .. })
    }
}

/// One local device the client owns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Device {
    /// Not opened: no renderer configured, or the output was left off.
    #[default]
    Off,
    /// Open, with the detail worth showing (rate, model path, device name).
    Ready(String),
    /// Wanted but unavailable, with the reason.
    Unavailable(String),
}

/// The client's own devices, as the footer shows them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Devices {
    pub avatar: Device,
    pub output: Device,
    pub input: Device,
}

/// A user-visible failure, conflict or refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// The action that failed, in the user's words.
    pub context: String,
    pub message: String,
    pub kind: NoticeKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    /// The request did not happen.
    Failed,
    /// The host's revision moved; the window refetched and the operator retries.
    Conflict,
    /// The credential or configuration was refused; retrying will not help.
    Denied,
}

/// Text the operator sent whose acceptance has not arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sending {
    pub request_key: String,
    pub text: String,
}

/// Text the host did not accept, kept with its key so a retry is not a
/// duplicate: the host deduplicates on `request_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsent {
    pub request_key: String,
    pub text: String,
    pub reason: String,
}

/// The selected companion's conversation.
#[derive(Clone, Debug, Default)]
pub struct Chat {
    /// The companion these messages belong to.
    pub companion_id: String,
    /// What the host recorded, oldest first.
    pub messages: Vec<w::Message>,
    pub loading: bool,
    pub error: Option<String>,
    pub sending: Vec<Sending>,
    pub unsent: Vec<Unsent>,
    /// The most recent turn for this companion.
    pub turn: Option<w::TurnEvent>,
    /// A stop the host accepted but has not finished. The window shows
    /// "停止を要求しました" until a terminal turn arrives, never "停止しました".
    pub stop_requested: bool,
}

/// Live speech-to-text recognition subtitle shown while listening.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTranscription {
    pub text: String,
    pub is_final: bool,
    pub is_speaking: bool,
}

/// What the settings probe shows about local capture, so the operator can tell
/// a dead microphone from a closed voice gate from a host that received nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Capture {
    /// RMS of the frames the microphone delivered.
    pub level: f32,
    /// Whether the voice gate is passing audio.
    pub open: bool,
    /// Wire bytes handed to the host since the last report.
    pub sent: usize,
}

/// A readback line the speech tab keeps after its request finished.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpeechReport {
    pub text: String,
    pub failed: bool,
}

/// What the window should refetch because a notification invalidated it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub history: bool,
    pub approvals: bool,
    pub mind: bool,
}

/// Everything the window draws.
#[derive(Debug)]
pub struct Model {
    pub link: Link,
    /// Monotonically incremented whenever the host connection leaves or enters
    /// `Link::Live`, so stale async replies from earlier connections are dropped.
    pub link_epoch: u64,
    /// The last snapshot: the single display copy of host state.
    pub snapshot: Option<w::Snapshot>,
    /// The companion the operator picked, or the host's active one.
    pub selected: Option<String>,
    pub chat: Chat,
    /// Streamed text per companion and host stream id.
    pub streaming: BTreeMap<(String, String), String>,
    pub notices: Vec<Notice>,
    pub devices: Devices,
    /// The provider catalog, fetched once from the host.
    pub providers: Option<Vec<w::LlmProvider>>,
    /// Declared model ids per connection id.
    pub models: BTreeMap<String, Vec<w::Model>>,
    /// The last connection check per connection id.
    pub checks: BTreeMap<String, w::ConnectionCheck>,
    pub audio_output_rms: f32,
    pub audio_input_rms: f32,
    pub companion_speaking: bool,
    pub mic_muted: bool,
    pub capture_mode: crate::audio::CaptureMode,
    pub ptt_active: bool,
    pub eyes_enabled: bool,
    #[cfg(windows)]
    pub computer_use_enabled: bool,
    pub eyes_status: Option<String>,
    pub output_muted: bool,
    pub avatar_scale: f32,
    pub transcription: Option<LiveTranscription>,
    /// Local capture state, for the speech settings probe.
    pub capture: Capture,
    /// The last line the speech probe showed.
    pub speech_report: Option<SpeechReport>,
    /// Text the recognition stream produced while the probe was open, oldest
    /// first. Distinct from `transcription`, which the pod clears on `is_final`.
    pub recognized: Vec<String>,
    pub minds: BTreeMap<String, w::MindView>,
    pub work_active: bool,
    /// Built-in companion starter templates loaded from the host.
    pub templates: Vec<w::CompanionTemplate>,
    /// Most recent sanitized template JSON exported for `(companion_id, json)`.
    pub exported_template: Option<(String, String)>,
    /// Latest ephemeral preview response before saving a new companion.
    pub preview_reply: Option<Result<String, String>>,
    /// Installed host plugins (`F12`).
    pub plugins: Vec<w::PluginInfo>,
}

impl Model {
    /// A window with nothing but the discovery path it will look for.
    pub fn new(path: String) -> Self {
        Self {
            link: Link::Unset {
                path,
                reason: "ホストがまだ起動していません".into(),
            },
            link_epoch: 0,
            snapshot: None,
            selected: None,
            chat: Chat::default(),
            streaming: BTreeMap::new(),
            notices: Vec::new(),
            devices: Devices::default(),
            providers: None,
            models: BTreeMap::new(),
            checks: BTreeMap::new(),
            audio_output_rms: 0.0,
            audio_input_rms: 0.0,
            companion_speaking: false,
            mic_muted: false,
            capture_mode: crate::audio::CaptureMode::Vad,
            ptt_active: false,
            eyes_enabled: false,
            #[cfg(windows)]
            computer_use_enabled: false,
            eyes_status: None,
            output_muted: false,
            avatar_scale: 100.0,
            transcription: None,
            capture: Capture::default(),
            speech_report: None,
            recognized: Vec::new(),
            minds: BTreeMap::new(),
            work_active: false,
            templates: Vec::new(),
            exported_template: None,
            preview_reply: None,
            plugins: Vec::new(),
        }
    }

    /// The snapshot, when one has arrived.
    pub fn snapshot(&self) -> Option<&w::Snapshot> {
        self.snapshot.as_ref()
    }

    /// The companion the window is showing.
    pub fn companion_id(&self) -> Option<&str> {
        self.selected
            .as_deref()
            .or_else(|| self.snapshot.as_ref()?.active_companion_id.as_deref())
            .or_else(|| {
                self.snapshot
                    .as_ref()?
                    .companions
                    .first()
                    .map(|item| item.id.as_str())
            })
    }

    /// The companion record for the shown id.
    pub fn companion(&self) -> Option<&w::Companion> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .companions
            .iter()
            .find(|companion| companion.id == id)
    }

    /// The activation the host holds for the shown companion.
    pub fn activation(&self) -> Option<&w::Activation> {
        let id = self.companion_id()?;
        self.snapshot
            .as_ref()?
            .activations
            .iter()
            .find(|activation| activation.companion_id == id)
    }

    /// Whether this client is the shown companion's active device.
    pub fn is_active_client(&self, client_id: &str) -> bool {
        self.activation()
            .and_then(|activation| activation.client_id.as_deref())
            == Some(client_id)
    }

    /// All companion IDs currently resident on `client_id`.
    pub fn resident_companions(&self, client_id: &str) -> Vec<String> {
        self.snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
            snapshot
                .activations
                .iter()
                .filter(|activation| activation.client_id.as_deref() == Some(client_id))
                .map(|activation| activation.companion_id.clone())
                .collect()
        })
    }

    /// Whether `companion_id` is currently resident on `client_id`.
    pub fn is_resident(&self, companion_id: &str, client_id: &str) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.activations.iter().any(|activation| {
                activation.companion_id == companion_id
                    && activation.client_id.as_deref() == Some(client_id)
            })
        })
    }

    /// The settings the window shows.
    pub fn settings(&self) -> Option<&w::Settings> {
        self.snapshot.as_ref()?.settings.as_option()
    }

    pub fn policy_texts(&self) -> &[w::PolicyText] {
        self.settings()
            .map_or(&[], |settings| settings.policy_texts.as_slice())
    }

    /// The pending approvals, oldest first.
    pub fn approvals(&self) -> &[w::ApprovalRequest] {
        self.snapshot
            .as_ref()
            .map_or(&[], |snapshot| snapshot.pending_approvals.as_slice())
    }

    /// Adds a user-visible failure, keeping only the recent ones.
    pub fn notice(
        &mut self,
        kind: NoticeKind,
        context: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.notices.push(Notice {
            context: context.into(),
            message: message.into(),
            kind,
        });
        if self.notices.len() > 8 {
            self.notices.remove(0);
        }
    }

    /// Folds one update into the view and reports what must be refetched.
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    pub fn apply(&mut self, update: Update) -> Effects {
        let mut effects = Effects::default();
        match update {
            Update::Scoped {
                epoch,
                seq: _,
                inner,
            } => {
                if epoch != self.link_epoch {
                    return Effects::default();
                }
                return self.apply(*inner);
            }
            Update::Link(link) => {
                let was_live = self.link.is_live();
                let is_live = link.is_live();
                let appeared = is_live && !was_live;
                if was_live != is_live {
                    self.link_epoch = self.link_epoch.saturating_add(1);
                }
                self.link = *link;
                if !self.link.is_live() {
                    // A preview belongs to the link that produced it. Keeping it
                    // would show a half answer as if it were still arriving.
                    self.streaming.clear();
                    self.companion_speaking = false;
                    self.audio_output_rms = 0.0;
                    self.transcription = None;
                    // The request may or may not have reached the host, so it
                    // moves to "unsent" with its key intact: a retry is safe.
                    for sending in std::mem::take(&mut self.chat.sending) {
                        self.chat.unsent.push(Unsent {
                            request_key: sending.request_key,
                            text: sending.text,
                            reason: "接続が切れたため結果不明です。再送しても二重にはなりません。"
                                .into(),
                        });
                    }
                }
                if appeared {
                    // A replayed stream cannot carry every delta, so the window
                    // refetches what the snapshot does not hold.
                    effects.history = true;
                    effects.approvals = true;
                }
            }
            Update::Notification(notification) => {
                merge_notification(self, *notification, &mut effects);
            }
            Update::Resync => {
                self.streaming.clear();
            }
            // The window decides what to open; nothing here changes.
            Update::Created(_) => {}
            Update::Devices(devices) => self.devices = *devices,
            Update::Failed { context, message } => {
                self.notice(NoticeKind::Failed, context, message);
            }
            Update::Conflict { context, message } => {
                self.notice(NoticeKind::Conflict, context, message);
            }
            Update::Denied { context, message } => {
                self.notice(NoticeKind::Denied, context, message);
            }
            Update::Accepted {
                request_key,
                request_id: _,
            } => {
                self.chat
                    .sending
                    .retain(|sending| sending.request_key != request_key);
            }
            Update::SendFailed {
                request_key,
                text,
                reason,
            } => {
                self.chat
                    .sending
                    .retain(|sending| sending.request_key != request_key);
                self.chat.unsent.push(Unsent {
                    request_key,
                    text,
                    reason,
                });
            }
            Update::History {
                companion_id,
                messages,
            } => {
                if self.chat.companion_id == companion_id {
                    self.chat.messages = messages
                        .into_iter()
                        .filter(is_user_facing_message)
                        .collect();
                    self.chat.loading = false;
                    self.chat.error = None;
                    // A recorded user message means the send landed, so its
                    // pending row is done.
                    self.chat.sending.retain(|sending| {
                        !self.chat.messages.iter().any(|message| {
                            message.role == w::MessageRole::User && message.text == sending.text
                        })
                    });
                }
            }
            Update::HistoryFailed {
                companion_id,
                message,
            } => {
                if self.chat.companion_id == companion_id {
                    self.chat.loading = false;
                    self.chat.error = Some(message);
                }
            }

            Update::Providers(providers) => self.providers = Some(providers),
            Update::Models {
                connection_id,
                models,
            } => {
                self.models.insert(connection_id, models);
            }
            Update::Checked {
                connection_id,
                check,
            } => {
                self.checks.insert(connection_id, check);
            }
            Update::Approvals(items) => {
                if let Some(snapshot) = self.snapshot.as_mut() {
                    snapshot.pending_approvals = items;
                }
            }
            Update::Resolved {
                request_id,
                rule_warning,
            } => {
                if let Some(snapshot) = self.snapshot.as_mut() {
                    snapshot
                        .pending_approvals
                        .retain(|approval| approval.request_id != request_id);
                }
                if let Some(warning) = rule_warning {
                    self.notice(NoticeKind::Failed, "許可ルールの保存", warning);
                }
            }
            Update::StopRequested { companion_id } => {
                if self.chat.companion_id == companion_id {
                    self.chat.stop_requested = true;
                }
            }
            Update::ApprovalStale { message } => {
                // Another device resolved it, or it expired. The card must stop
                // offering a decision the host would refuse.
                self.notice(NoticeKind::Conflict, "承認", message);
                effects.approvals = true;
            }
            Update::Gone { companion_id } => {
                if let Some(snapshot) = self.snapshot.as_mut() {
                    snapshot
                        .companions
                        .retain(|companion| companion.id != companion_id);
                    snapshot
                        .activations
                        .retain(|activation| activation.companion_id != companion_id);
                }
                self.minds.remove(&companion_id);
                if self.selected.as_deref() == Some(companion_id.as_str()) {
                    self.selected = None;
                    self.chat = Chat::default();
                    self.streaming.clear();
                }
            }
            Update::AudioLevel {
                output,
                input,
                speaking,
            } => {
                self.audio_output_rms = output;
                self.audio_input_rms = input;
                self.companion_speaking = speaking;
            }
            Update::EyesEnabled(enabled) => {
                self.eyes_enabled = enabled;
                self.eyes_status = None;
            }
            Update::EyesStatus(status) => self.eyes_status = status,
            #[cfg(windows)]
            Update::ComputerUseEnabled(enabled) => self.computer_use_enabled = enabled,
            Update::AudioMute {
                mic_muted,
                output_muted,
            } => {
                self.mic_muted = mic_muted;
                self.output_muted = output_muted;
                if mic_muted {
                    self.audio_input_rms = 0.0;
                    self.transcription = None;
                }
                if output_muted {
                    self.audio_output_rms = 0.0;
                }
            }
            Update::CaptureModeChanged { mode, ptt_active } => {
                self.capture_mode = mode;
                self.ptt_active = ptt_active;
            }
            Update::Plugins(plugins) => {
                self.plugins = plugins;
            }
            Update::Transcription {
                text,
                is_final,
                is_speaking,
            } => {
                if is_final {
                    // The probe keeps what the recognizer produced; the pod's
                    // subtitle is cleared either way.
                    let text = text.trim();
                    if !text.is_empty() {
                        self.recognized.push(text.to_owned());
                        drop(
                            self.recognized
                                .split_off(self.recognized.len().saturating_sub(20)),
                        );
                    }
                }
                if is_final || (text.trim().is_empty() && !is_speaking) {
                    self.transcription = None;
                } else {
                    self.transcription = Some(LiveTranscription {
                        text,
                        is_final,
                        is_speaking,
                    });
                }
            }
            Update::Capture { level, open, sent } => {
                self.capture = Capture { level, open, sent };
            }
            Update::Spoke => {
                self.speech_report = Some(SpeechReport {
                    text: "読み上げを送りました。音が鳴らない場合は、この端末の出力と読み上げ接続を確認してください。".into(),
                    failed: false,
                });
            }
            Update::SpokeFailed { message } => {
                self.speech_report = Some(SpeechReport {
                    text: message.clone(),
                    failed: true,
                });
            }
            Update::MindLoaded { companion_id, view } => {
                self.minds.insert(companion_id, *view);
            }
            Update::PolicyRules(rules) => {
                if let Some(snapshot) = self.snapshot.as_mut() {
                    snapshot.policy_rules = rules;
                }
            }
            Update::PolicyTexts(texts) => {
                if let Some(snapshot) = self.snapshot.as_mut() {
                    let mut settings = snapshot.settings.take().unwrap_or_default();
                    settings.policy_texts = texts;
                    snapshot.settings = Some(settings).into();
                }
            }
            Update::AvatarScaleChanged(scale) => {
                self.avatar_scale = scale;
            }
            Update::CompanionTemplates(items) => {
                self.templates = items;
            }
            Update::CompanionExported { companion_id, json } => {
                self.exported_template = Some((companion_id, json));
            }
            Update::CompanionPreview { reply, error } => {
                self.preview_reply = Some(match reply {
                    Some(text) => Ok(text),
                    None => Err(error.unwrap_or_else(|| "プレビューに失敗しました".into())),
                });
            }
            // The window clears the field that held the secret; nothing about a
            // credential is kept in the model, and a resnapshot is the window's
            // own request.
            Update::SetupFinished { .. }
            | Update::CredentialRegistered { connection_id: _ }
            | Update::Resnapshot => {}
        }
        effects
    }
}

/// Whether a host message should be visible in user-facing chat bubbles.
pub fn is_user_facing_message(message: &w::Message) -> bool {
    if !message.work_id.trim().is_empty() {
        return false;
    }
    if matches!(
        message.role.as_known(),
        Some(w::MessageRole::Agent | w::MessageRole::Context)
    ) {
        return false;
    }
    let trimmed = message.text.trim();
    !(trimmed.starts_with("[Output of ")
        || trimmed.starts_with("[作業報告")
        || trimmed.starts_with("[作業結果")
        || trimmed.starts_with("[hand ")
        || trimmed.starts_with("The hand finished"))
}

/// Merges one host notification into the display copy.
// TODO(Phase 1-3): split during refactoring.
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn merge_notification(model: &mut Model, notification: w::Notification, effects: &mut Effects) {
    match notification.event {
        Some(w::notification::Event::InitialState(snapshot)) => {
            let mut snapshot = *snapshot;
            snapshot
                .companions
                .sort_by(|left, right| left.name.cmp(&right.name));
            model.work_active = snapshot
                .work_sessions
                .iter()
                .any(|session| matches!(session.status.as_known(), Some(w::WorkStatus::Active)));
            model.snapshot = Some(snapshot);
            effects.approvals = false;
        }
        Some(w::notification::Event::Health(health)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.health = Some(*health).into();
            }
        }
        Some(w::notification::Event::Companion(companion)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_by_revision(
                    &mut snapshot.companions,
                    *companion,
                    |item| item.id.clone(),
                    |item| item.revision,
                );
                snapshot
                    .companions
                    .sort_by(|left, right| left.name.cmp(&right.name));
            }
        }
        Some(w::notification::Event::MessageCreated(created)) => {
            if let Some(message) = created.message.into_option()
                && message.companion_id == model.chat.companion_id
                && is_user_facing_message(&message)
            {
                // The recorded message is the answer; any preview for this
                // companion is now stale.
                model
                    .streaming
                    .retain(|(companion, _), _| companion != &message.companion_id);
                merge_message(&mut model.chat.messages, message);
            }
        }
        Some(w::notification::Event::MessageDelta(delta)) => {
            if delta.companion_id == model.chat.companion_id {
                let text = model
                    .streaming
                    .entry((delta.companion_id.clone(), delta.stream_id.clone()))
                    .or_default();
                text.push_str(&delta.delta);
            }
        }
        Some(w::notification::Event::TurnEvent(turn)) => {
            if turn.companion_id == model.chat.companion_id {
                let terminal = matches!(
                    turn.status.as_known(),
                    Some(w::TurnStatus::Answered)
                        | Some(w::TurnStatus::Stopped)
                        | Some(w::TurnStatus::Failed)
                );
                if terminal {
                    model.streaming.clear();
                    model.chat.stop_requested = false;
                }
                if turn.status.as_known() == Some(w::TurnStatus::Failed)
                    && let Some(failure) = turn.failure.as_option()
                    && !failure.message.trim().is_empty()
                {
                    model.notice(
                        NoticeKind::Failed,
                        "応答に失敗しました",
                        failure.message.clone(),
                    );
                }
                model.chat.turn = Some(*turn);
            }
        }
        Some(w::notification::Event::ApprovalRequested(requested)) => {
            if let Some(request) = requested.request.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_approval(&mut snapshot.pending_approvals, request);
            } else {
                effects.approvals = true;
            }
        }
        Some(w::notification::Event::PolicyRulesUpdated(updated)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                snapshot.policy_rules = updated.rules;
            }
        }
        Some(w::notification::Event::SettingsUpdated(updated)) => {
            if let Some(settings) = updated.settings.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
                && snapshot
                    .settings
                    .as_option()
                    .is_none_or(|current| current.revision <= settings.revision)
            {
                snapshot.settings = Some(settings).into();
            }
        }
        Some(w::notification::Event::ClientUpdated(updated)) => {
            if let Some(client) = updated.client.into_option()
                && let Some(snapshot) = model.snapshot.as_mut()
            {
                upsert_by_revision(
                    &mut snapshot.clients,
                    client,
                    |item| item.id.clone(),
                    |item| item.revision,
                );
            }
        }
        Some(w::notification::Event::ActivationUpdated(activation)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                upsert_activation(&mut snapshot.activations, *activation);
            }
        }
        Some(w::notification::Event::PolicyTextsUpdated(updated)) => {
            if let Some(snapshot) = model.snapshot.as_mut() {
                let mut settings = snapshot.settings.take().unwrap_or_default();
                settings.policy_texts = updated.texts;
                snapshot.settings = Some(settings).into();
            }
        }
        // Work stays behind the companion's dialogue; the window has no surface
        // a work event would refresh.
        Some(w::notification::Event::WorkEvent(work)) => {
            model.work_active = match &work.detail {
                Some(w::work_event::Detail::RunStarted(_)) => true,
                Some(w::work_event::Detail::StatusChanged(changed)) => {
                    matches!(changed.status.as_known(), Some(w::WorkStatus::Active))
                }
                Some(w::work_event::Detail::RunFinished(finished)) => matches!(
                    finished.state.as_known(),
                    Some(
                        w::RunStatus::Starting | w::RunStatus::Running | w::RunStatus::WaitingInput
                    )
                ),
                None => false,
            };
        }
        Some(w::notification::Event::ResyncRequired(_)) => {
            model.streaming.clear();
        }
        Some(w::notification::Event::Generic(event)) => {
            if event.kind == "notice"
                && let Some(document) = event.payload.as_option()
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(&document.json)
                && let Some(message) = value.get("message").and_then(|item| item.as_str())
            {
                let code = value
                    .get("code")
                    .and_then(|item| item.as_str())
                    .unwrap_or_default();
                match code {
                    "approval_resolved" => {
                        let req_id = message.split(':').next().unwrap_or_default().trim();
                        if !req_id.is_empty()
                            && let Some(snapshot) = model.snapshot.as_mut()
                        {
                            snapshot
                                .pending_approvals
                                .retain(|item| item.request_id != req_id);
                        }
                    }
                    "approval_required" | "host_ready" => {}
                    "model_unavailable" => {
                        let has_enabled_llm = model.settings().is_some_and(|settings| {
                            settings
                                .connections
                                .iter()
                                .any(|c| c.kind == w::ConnectionKind::Llm && c.enabled)
                        });
                        if has_enabled_llm {
                            model.notice(
                                NoticeKind::Failed,
                                "ホストからの通知",
                                message.to_owned(),
                            );
                        }
                    }
                    _ => {
                        model.notice(NoticeKind::Failed, "ホストからの通知", message.to_owned());
                    }
                }
            }
        }
        // Observation only: execution flows through `ClientService.DeviceStream`.
        Some(w::notification::Event::DeviceOperation(_))
        | Some(w::notification::Event::DeviceOperationResult(_)) => {}
        Some(w::notification::Event::MindUpdated(updated)) => {
            effects.mind |= model.companion_id() == Some(updated.companion_id.as_str())
                || (!updated.companion_id.is_empty()
                    && model.chat.companion_id == updated.companion_id);
        }
        None => {}
    }
}

/// Inserts or replaces one message, keeping the log ordered by the host's
/// sequence. A message that arrives twice is replaced, not appended twice.
pub fn merge_message(messages: &mut Vec<w::Message>, message: w::Message) {
    if let Some(existing) = messages
        .iter_mut()
        .find(|existing| existing.id == message.id)
    {
        *existing = message;
        return;
    }
    let at = messages.partition_point(|existing| existing.sequence <= message.sequence);
    messages.insert(at, message);
}

/// Keeps a pending approval current, and drops it once it is no longer pending.
pub fn upsert_approval(approvals: &mut Vec<w::ApprovalRequest>, request: w::ApprovalRequest) {
    let present = approvals
        .iter()
        .position(|approval| approval.request_id == request.request_id);
    if request.status == w::ApprovalStatus::Pending {
        match present {
            Some(index) => approvals[index] = request,
            None => approvals.push(request),
        }
    } else if let Some(index) = present {
        approvals.remove(index);
    }
}

/// Replaces `items[index]` when the incoming revision is not older.
fn upsert_by_revision<T>(
    items: &mut Vec<T>,
    item: T,
    key: impl Fn(&T) -> String,
    revision: impl Fn(&T) -> u64,
) {
    let id = key(&item);
    match items.iter().position(|existing| key(existing) == id) {
        Some(index) if revision(&items[index]) <= revision(&item) => items[index] = item,
        Some(_) => {}
        None => items.push(item),
    }
}

/// Keeps one activation per companion, latest notification winning.
fn upsert_activation(activations: &mut Vec<w::Activation>, activation: w::Activation) {
    match activations
        .iter()
        .position(|current| current.companion_id == activation.companion_id)
    {
        Some(index) => activations[index] = activation,
        None => activations.push(activation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model::new("connection.json".into())
    }

    fn snapshot() -> w::Snapshot {
        w::Snapshot {
            server_version: "1".into(),
            pending_approvals: Vec::new(),
            ..Default::default()
        }
    }

    fn notification(event: w::notification::Event) -> w::Notification {
        w::Notification {
            kind: "test".into(),
            event: Some(event),
            ..Default::default()
        }
    }

    fn message(id: &str, sequence: u64, role: w::MessageRole, text: &str) -> w::Message {
        w::Message {
            id: id.into(),
            companion_id: "ene".into(),
            role: role.into(),
            text: text.into(),
            sequence,
            ..Default::default()
        }
    }

    fn with_chat() -> Model {
        let mut model = model();
        model.chat.companion_id = "ene".into();
        model
    }

    #[test]
    fn a_delta_is_a_preview_until_the_host_records_the_message() {
        let mut model = with_chat();
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                companion_id: "ene".into(),
                stream_id: "stream".into(),
                delta: "こん".into(),
                ..Default::default()
            })),
        ))));
        assert!(model.chat.messages.is_empty(), "a delta is not a message");
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::MessageDelta(Box::new(w::MessageDelta {
                companion_id: "ene".into(),
                stream_id: "stream".into(),
                delta: "にちは".into(),
                ..Default::default()
            })),
        ))));
        assert_eq!(
            model
                .streaming
                .get(&("ene".to_owned(), "stream".to_owned()))
                .map(String::as_str),
            Some("こんにちは")
        );
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                message: Some(message("a", 9, w::MessageRole::Assistant, "こんにちは")).into(),
                ..Default::default()
            })),
        ))));
        assert_eq!(model.chat.messages.len(), 1);
        assert!(
            model.streaming.is_empty(),
            "the preview gives way to the message"
        );
    }

    #[test]
    fn the_same_message_twice_is_one_message_in_sequence_order() {
        let mut model = with_chat();
        let created = |id: &str, sequence: u64| {
            Update::Notification(Box::new(notification(
                w::notification::Event::MessageCreated(Box::new(w::MessageCreated {
                    message: Some(message(id, sequence, w::MessageRole::User, id)).into(),
                    ..Default::default()
                })),
            )))
        };
        model.apply(created("b", 7));
        model.apply(created("a", 5));
        model.apply(created("b", 7));
        let ids: Vec<&str> = model
            .chat
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["a", "b"],
            "replay replaces, and order follows sequence"
        );
    }

    #[test]
    fn a_turn_event_for_another_companion_does_not_touch_the_shown_chat() {
        let mut model = with_chat();
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: "other".into(),
                turn_id: "1".into(),
                status: w::TurnStatus::Failed.into(),
                ..Default::default()
            })),
        ))));
        assert!(model.chat.turn.is_none());
    }

    #[test]
    fn a_terminal_turn_drops_the_preview() {
        let mut model = with_chat();
        model
            .streaming
            .insert(("ene".into(), "stream".into()), "半分".into());
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::TurnEvent(Box::new(w::TurnEvent {
                companion_id: "ene".into(),
                turn_id: "1".into(),
                status: w::TurnStatus::Stopped.into(),
                ..Default::default()
            })),
        ))));
        assert!(model.streaming.is_empty(), "a stopped turn has no preview");
    }

    #[test]
    fn a_resolved_approval_leaves_the_pending_list() {
        let mut model = model();
        model.snapshot = Some(snapshot());
        let pending = |status: w::ApprovalStatus| w::ApprovalRequest {
            request_id: "r1".into(),
            status: status.into(),
            ..Default::default()
        };
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::ApprovalRequested(Box::new(w::ApprovalRequested {
                request: Some(pending(w::ApprovalStatus::Pending)).into(),
                ..Default::default()
            })),
        ))));
        assert_eq!(model.approvals().len(), 1);
        model.apply(Update::Resolved {
            request_id: "r1".into(),
            rule_warning: None,
        });
        assert!(
            model.approvals().is_empty(),
            "a resolved approval is not pending"
        );
        model.apply(Update::Resolved {
            request_id: "r2".into(),
            rule_warning: Some("rule save failed".into()),
        });
        assert!(
            model
                .notices
                .iter()
                .any(|notice| notice.message == "rule save failed"),
            "AlwaysApprove save failures must be surfaced"
        );
    }

    #[test]
    fn f16_policy_texts_updated_notification_updates_settings() {
        let mut model = model();
        model.snapshot = Some(snapshot());
        let entry = w::PolicyText {
            id: "pt-1".into(),
            text: "外部送信は毎回確認".into(),
            ..Default::default()
        };
        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::PolicyTextsUpdated(Box::new(w::PolicyTextsUpdated {
                texts: vec![entry.clone()],
                ..Default::default()
            })),
        ))));
        assert_eq!(model.policy_texts().len(), 1);
        assert_eq!(model.policy_texts()[0].text, "外部送信は毎回確認");
    }

    #[test]
    fn an_older_companion_revision_never_replaces_a_newer_one() {
        let mut model = model();
        model.snapshot = Some(snapshot());
        let companion = |revision: u64, name: &str| w::Companion {
            id: "ene".into(),
            name: name.into(),
            revision,
            ..Default::default()
        };
        let update = |revision: u64, name: &str| {
            Update::Notification(Box::new(notification(w::notification::Event::Companion(
                Box::new(companion(revision, name)),
            ))))
        };
        model.apply(update(2, "新しい"));
        model.apply(update(1, "古い"));
        let name = &model.snapshot().unwrap().companions[0].name;
        assert_eq!(name, "新しい");
    }

    #[test]
    fn older_settings_are_ignored_so_a_replay_cannot_roll_the_window_back() {
        let mut model = model();
        model.snapshot = Some(snapshot());
        let settings = |revision: u64| w::Settings {
            revision,
            ..Default::default()
        };
        let update = |revision: u64| {
            Update::Notification(Box::new(notification(
                w::notification::Event::SettingsUpdated(Box::new(w::SettingsUpdated {
                    settings: Some(settings(revision)).into(),
                    ..Default::default()
                })),
            )))
        };
        model.apply(update(4));
        model.apply(update(3));
        assert_eq!(model.settings().unwrap().revision, 4);
    }

    #[test]
    fn losing_the_link_keeps_the_data_but_moves_in_flight_sends_to_unsent() {
        let mut model = with_chat();
        model.chat.sending.push(Sending {
            request_key: "key".into(),
            text: "送った".into(),
        });
        model.apply(Update::Link(Box::new(Link::Lost {
            reason: "closed".into(),
            retry_in: Duration::from_secs(1),
        })));
        assert!(!model.link.is_live());
        assert!(model.chat.sending.is_empty());
        assert_eq!(model.chat.unsent.len(), 1);
        assert_eq!(
            model.chat.unsent[0].request_key, "key",
            "the key survives so a retry is idempotent"
        );
    }

    #[test]
    fn a_late_failure_does_not_clobber_new_text() {
        let mut model = with_chat();
        model.chat.sending.push(Sending {
            request_key: "key".into(),
            text: "前の入力".into(),
        });
        model.apply(Update::SendFailed {
            request_key: "key".into(),
            text: "前の入力".into(),
            reason: "保存されませんでした".into(),
        });
        assert!(
            model
                .chat
                .unsent
                .iter()
                .any(|unsent| unsent.text == "前の入力")
        );
        assert!(model.chat.sending.is_empty());
    }

    #[test]
    fn a_recorded_user_message_closes_its_pending_row() {
        let mut model = with_chat();
        model.chat.sending.push(Sending {
            request_key: "key".into(),
            text: "こんにちは".into(),
        });
        model.apply(Update::History {
            companion_id: "ene".into(),
            messages: vec![message("a", 1, w::MessageRole::User, "こんにちは")],
        });
        assert!(model.chat.sending.is_empty(), "the host recorded the input");
    }

    #[test]
    fn becoming_live_after_a_drop_refetches_what_the_snapshot_cannot_carry() {
        let mut model = model();
        model.apply(Update::Link(Box::new(Link::Live {
            version: "1".into(),
        })));
        model.apply(Update::Link(Box::new(Link::Lost {
            reason: "closed".into(),
            retry_in: Duration::from_secs(1),
        })));
        let effects = model.apply(Update::Link(Box::new(Link::Live {
            version: "1".into(),
        })));
        assert!(effects.history && effects.approvals);
    }

    #[test]
    fn audio_levels_and_transcription_reset_when_link_drops() {
        let mut model = with_chat();
        model.apply(Update::Link(Box::new(Link::Live {
            version: "1".into(),
        })));
        model.apply(Update::AudioLevel {
            output: 0.4,
            input: 0.2,
            speaking: true,
        });
        model.apply(Update::Transcription {
            text: "こんにちは".into(),
            is_final: false,
            is_speaking: true,
        });
        assert!(model.companion_speaking);
        assert!(model.transcription.is_some());

        model.apply(Update::Link(Box::new(Link::Lost {
            reason: "closed".into(),
            retry_in: Duration::from_secs(1),
        })));
        assert!(!model.companion_speaking);
        assert_eq!(model.audio_output_rms, 0.0);
        assert!(model.transcription.is_none());
    }

    #[test]
    fn mind_updated_requests_refresh_only_for_active_companion() {
        let mut model = with_chat();
        let other = model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
                companion_id: "other".into(),
                ..Default::default()
            })),
        ))));
        assert!(!other.mind);

        let mine = model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::MindUpdated(Box::new(w::MindUpdated {
                companion_id: "ene".into(),
                ..Default::default()
            })),
        ))));
        assert!(mine.mind);
    }

    #[test]
    fn internal_work_messages_are_hidden_and_work_event_tracks_active_status() {
        let mut model = with_chat();
        let internal = w::Message {
            id: "w1".into(),
            companion_id: "ene".into(),
            work_id: "work-1".into(),
            role: w::MessageRole::Agent.into(),
            text: "[作業報告 (work_id: work-1)] ...".into(),
            sequence: 1,
            ..Default::default()
        };
        assert!(!is_user_facing_message(&internal));
        model.apply(Update::History {
            companion_id: "ene".into(),
            messages: vec![internal, message("u1", 2, w::MessageRole::User, "調べて")],
        });
        assert_eq!(model.chat.messages.len(), 1);
        assert_eq!(model.chat.messages[0].text, "調べて");

        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
                work_id: "work-1".into(),
                run_id: "run-1".into(),
                kind: w::WorkEventKind::RunStarted.into(),
                detail: Some(w::work_event::Detail::RunStarted(Box::default())),
                ..Default::default()
            })),
        ))));
        assert!(model.work_active);

        model.apply(Update::Notification(Box::new(notification(
            w::notification::Event::WorkEvent(Box::new(w::WorkEvent {
                work_id: "work-1".into(),
                run_id: "run-1".into(),
                kind: w::WorkEventKind::RunFinished.into(),
                detail: Some(w::work_event::Detail::RunFinished(Box::new(
                    w::WorkRunFinished {
                        state: w::RunStatus::Completed.into(),
                        ..Default::default()
                    },
                ))),
                ..Default::default()
            })),
        ))));
        assert!(!model.work_active);
    }

    #[test]
    fn q04_stale_epoch_scoped_updates_are_discarded_after_reconnect() {
        let mut model = model();
        model.apply(Update::Link(Box::new(Link::Live {
            version: "1".into(),
        })));
        let first_epoch = model.link_epoch;
        model.apply(Update::Models {
            connection_id: "c1".into(),
            models: vec![w::Model {
                id: "m-fresh".into(),
                ..Default::default()
            }],
        });

        model.apply(Update::Link(Box::new(Link::Lost {
            reason: "drop".into(),
            retry_in: Duration::from_secs(1),
        })));
        model.apply(Update::Link(Box::new(Link::Live {
            version: "2".into(),
        })));
        assert!(model.link_epoch > first_epoch);

        model.apply(Update::Scoped {
            epoch: first_epoch,
            seq: 1,
            inner: Box::new(Update::Models {
                connection_id: "c1".into(),
                models: vec![w::Model {
                    id: "m-stale".into(),
                    ..Default::default()
                }],
            }),
        });
        assert_eq!(
            model.models.get("c1").unwrap()[0].id,
            "m-fresh",
            "stale epoch response must not overwrite state after reconnect"
        );
    }

    #[test]
    fn f01_r03_resident_companions_queries_multiple_activations_on_one_client() {
        let mut model = model();
        let mut snap = snapshot();
        snap.activations = vec![
            w::Activation {
                companion_id: "ene".into(),
                client_id: Some("desk-a".into()),
                ..Default::default()
            },
            w::Activation {
                companion_id: "miku".into(),
                client_id: Some("desk-a".into()),
                ..Default::default()
            },
            w::Activation {
                companion_id: "kai".into(),
                client_id: Some("desk-b".into()),
                ..Default::default()
            },
        ];
        model.snapshot = Some(snap);
        assert_eq!(model.resident_companions("desk-a"), vec!["ene", "miku"]);
        assert!(model.is_resident("miku", "desk-a"));
        assert!(!model.is_resident("kai", "desk-a"));
    }
}
