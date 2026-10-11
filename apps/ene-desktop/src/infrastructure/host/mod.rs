//! Authenticated local transport. No UI object or provider credential lives here.
use ene_api_client::{ApiClient as Http, IdempotencyKey};
#[cfg(test)]
use ene_api_types::host::Replica;
use ene_api_types::host::{Message, RuntimeConnection, Snapshot};
use serde_json::Value;
use std::{fs, path::Path, sync::mpsc, time::Duration};
use tokio::sync::mpsc as channel;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod contracts;
pub use contracts::{
    ActionEvent, ActionResultEnvelope, Admission, Command, ConnectionPhase, CredentialEdit, Event,
    Feedback, OperationTarget, Request, RequestContext,
};

struct ConnectionDraft {
    provider: String,
    id: String,
    draft: String,
}
pub struct Controller {
    commands: channel::Sender<Request>,
    pub events: mpsc::Receiver<Event>,
    latest_requests:
        std::cell::RefCell<std::collections::BTreeMap<(String, OperationTarget), RequestContext>>,
    latest_view: std::sync::Arc<std::sync::Mutex<Option<Event>>>,
    creation_ids: std::cell::RefCell<std::collections::BTreeMap<String, ConnectionDraft>>,
    session: std::sync::Arc<std::sync::Mutex<Option<ene_api_client::SessionIdentity>>>,
    stop: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Controller {
    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, channel::Receiver<Request>) {
        let (commands, receiver) = channel::channel(64);
        let (_, events) = mpsc::channel();
        (
            Self {
                commands,
                events,
                latest_requests: Default::default(),
                latest_view: Default::default(),
                creation_ids: Default::default(),
                session: std::sync::Arc::new(std::sync::Mutex::new(Some(
                    ene_api_client::SessionIdentity {
                        host_id: "fixture".into(),
                        epoch: "fixture".into(),
                        subscription: "fixture".into(),
                    },
                ))),
                stop: CancellationToken::new(),
                thread: None,
            },
            receiver,
        )
    }
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::spawn(wake, false)
    }
    pub fn monitor(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self::spawn(wake, true)
    }
    fn spawn(wake: impl Fn() + Send + Sync + 'static, monitoring: bool) -> Self {
        let (commands, receiver) = channel::channel(64);
        let (sender, events) = mpsc::channel();
        let latest_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let projected = latest_view.clone();
        let session = std::sync::Arc::new(std::sync::Mutex::new(None));
        let connected_session = session.clone();
        let stop = CancellationToken::new();
        let token = stop.clone();
        let thread = std::thread::spawn(move || {
            let emit = move |event| {
                match &event {
                    Event::Session(identity) => {
                        *connected_session.lock().expect("session lock") = Some(identity.clone());
                    }
                    Event::Connection(phase, _) if *phase != ConnectionPhase::Connected => {
                        *connected_session.lock().expect("session lock") = None;
                    }
                    _ => {}
                }
                if matches!(&event, Event::View(..)) {
                    *projected.lock().expect("view lock") = Some(event);
                    wake();
                } else if sender.send(event).is_ok() {
                    wake();
                }
            };
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(async {
                    let task = async {
                        if monitoring {
                            monitor::run(receiver, &emit).await;
                        } else {
                            run(receiver, &emit).await;
                        }
                    };
                    tokio::select! { biased; _=token.cancelled()=>{}, _=task=>{} }
                }),
                Err(_) if monitoring => emit(Event::Runtime(monitor::RuntimeStatus::unavailable(
                    "ホストの状態確認を開始できません。アプリを起動し直してください。".into(),
                ))),
                Err(_) => emit(Event::Connection(
                    ConnectionPhase::Stopped,
                    "通信処理を起動できません。もう一度接続してください。".into(),
                )),
            }
        });
        Self {
            commands,
            events,
            latest_requests: std::cell::RefCell::new(std::collections::BTreeMap::new()),
            latest_view,
            creation_ids: std::cell::RefCell::new(std::collections::BTreeMap::new()),
            stop,
            session,
            thread: Some(thread),
        }
    }
    pub fn take_view(&self) -> Option<Event> {
        self.latest_view.lock().expect("view lock").take()
    }
    pub fn is_current(&self, request: &RequestContext) -> bool {
        self.latest_requests
            .borrow()
            .get(&(request.companion.clone(), request.target.clone()))
            .map(|context| &context.id)
            == Some(&request.id)
            && self.session_identity().as_ref() == Some(&request.session)
    }
    pub fn saved(&self, request: &RequestContext) {
        if self.is_current(request) && matches!(request.target, OperationTarget::Configuration(_)) {
            self.creation_ids.borrow_mut().remove(&request.companion);
        }
    }
    pub fn discard_connection_draft(&self, companion: &str) {
        self.creation_ids.borrow_mut().remove(companion);
    }
    pub fn request_at(
        &self,
        companion: &str,
        command: Command,
        draft_stamp: crate::host_model::DraftStamp,
    ) -> Option<String> {
        let mut request = Request::new(companion, command);
        if let Command::Configure {
            connection_id: None,
            creation_id,
            provider,
            ..
        } = &mut request.command
        {
            let mut identities = self.creation_ids.borrow_mut();
            let draft = identities
                .entry(companion.into())
                .or_insert_with(|| ConnectionDraft {
                    provider: provider.clone(),
                    id: Uuid::new_v4().to_string(),
                    draft: draft_stamp.id.clone(),
                });
            if draft.provider != *provider || draft.draft != draft_stamp.id {
                *draft = ConnectionDraft {
                    provider: provider.clone(),
                    id: Uuid::new_v4().to_string(),
                    draft: draft_stamp.id.clone(),
                };
            }
            *creation_id = Some(draft.id.clone());
        }
        let id = Uuid::new_v4().to_string();
        let policy = request.command.policy();
        if let Some(target) = policy.target {
            request.context = Some(RequestContext {
                companion: companion.into(),
                id: id.clone(),
                target,
                draft: draft_stamp,
                resource: policy.resource,
                saved_resource: policy.saved_resource,
                session: self.session_identity()?,
            });
        }
        let context = request.context.clone();
        self.commands.try_send(request).ok()?;
        if let Some(context) = context {
            self.latest_requests
                .borrow_mut()
                .insert((companion.into(), context.target.clone()), context);
        }
        Some(id)
    }
    pub fn context(&self, id: &str) -> Option<RequestContext> {
        self.latest_requests
            .borrow()
            .values()
            .find(|context| context.id == id)
            .cloned()
    }
    pub fn session_identity(&self) -> Option<ene_api_client::SessionIdentity> {
        self.session.lock().expect("session lock").clone()
    }
    pub fn request(&self, companion: &str, command: Command) -> bool {
        self.request_at(companion, command, crate::host_model::DraftStamp::default())
            .is_some()
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

mod launch;
mod monitor;
mod mutations;
pub use monitor::RuntimeStatus;
mod session;
mod settings;
mod transport;
use session::run;
#[cfg(test)]
use settings::action;
use settings::{ActionResult, action_with_key};
use transport::{Error, descriptor};
#[cfg(test)]
mod regression_tests;
#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../../../crates/ene-api-client/tests/support/mod.rs"]
mod connect_fixture;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
