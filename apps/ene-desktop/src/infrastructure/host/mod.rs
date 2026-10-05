//! Authenticated local transport. No UI object or provider credential lives here.
use ene_api_client::{ApiClient as Http, IdempotencyKey, ServerEvent};
use ene_api_types::host::{Message, Page, Replica, RuntimeConnection, Snapshot};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::mpsc,
    time::Duration,
};
use tokio::sync::mpsc as channel;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod contracts;
pub use contracts::{
    Command, ConnectionPhase, CredentialEdit, Event, OperationTarget, Request, RequestContext,
};

pub struct Controller {
    commands: channel::UnboundedSender<Request>,
    pub events: mpsc::Receiver<Event>,
    latest_requests:
        std::cell::RefCell<std::collections::BTreeMap<(String, OperationTarget), String>>,
    latest_view: std::sync::Arc<std::sync::Mutex<Option<Event>>>,
    creation_ids: std::cell::RefCell<std::collections::BTreeMap<String, (String, String)>>,
    stop: CancellationToken,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Controller {
    #[cfg(test)]
    pub(crate) fn test_channel() -> (Self, channel::UnboundedReceiver<Request>) {
        let (commands, receiver) = channel::unbounded_channel();
        let (_, events) = mpsc::channel();
        (
            Self {
                commands,
                events,
                latest_requests: Default::default(),
                latest_view: Default::default(),
                creation_ids: Default::default(),
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
        let (commands, receiver) = channel::unbounded_channel();
        let (sender, events) = mpsc::channel();
        let latest_view = std::sync::Arc::new(std::sync::Mutex::new(None));
        let projected = latest_view.clone();
        let stop = CancellationToken::new();
        let token = stop.clone();
        let thread = std::thread::spawn(move || {
            let emit = move |event| {
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
            == Some(&request.id)
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
        edit_revision: i32,
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
                .or_insert_with(|| (provider.clone(), Uuid::new_v4().to_string()));
            if draft.0 != *provider {
                *draft = (provider.clone(), Uuid::new_v4().to_string());
            }
            *creation_id = Some(draft.1.clone());
        }
        let id = Uuid::new_v4().to_string();
        if let Some(policy) = request.command.policy() {
            let target = policy.target;
            self.latest_requests
                .borrow_mut()
                .insert((companion.into(), target.clone()), id.clone());
            request.context = Some(RequestContext {
                companion: companion.into(),
                id: id.clone(),
                target,
                edit_revision,
            });
        }
        self.commands.send(request).ok().map(|_| id)
    }
    pub fn request(&self, companion: &str, command: Command) -> bool {
        self.request_at(companion, command, 0).is_some()
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
use settings::{ActionResult, action};
use transport::{Error, descriptor};
#[cfg(test)]
mod regression_tests;
#[cfg(test)]
mod tests;
