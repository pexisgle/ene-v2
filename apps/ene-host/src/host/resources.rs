//! Shared host resources: health, live device streams and perception.
use super::error::{HostError, Result};
use bevy_ecs::prelude::*;
use ene_domain::{SubscriptionId, client_view::HostHealth};
use ene_rpc as w;
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

/// A fatal state means the host must not accept new work.
#[derive(Clone, Resource, Default)]
pub(crate) struct HostState(Arc<Mutex<Lifecycle>>);

#[derive(Default)]
enum Lifecycle {
    #[default]
    Ready,
    Closing,
    Failed(String),
}

impl HostState {
    pub(crate) fn health(&self) -> HostHealth {
        match &*self.0.lock() {
            Lifecycle::Ready => HostHealth::Ready,
            Lifecycle::Closing => HostHealth::Stopping,
            Lifecycle::Failed(_) => HostHealth::RecoveryRequired,
        }
    }

    pub(crate) fn ready(&self) -> Result<()> {
        match &*self.0.lock() {
            Lifecycle::Ready => Ok(()),
            Lifecycle::Closing => Err(HostError::Unavailable("host is stopping".into())),
            Lifecycle::Failed(reason) => Err(HostError::Unavailable(reason.clone())),
        }
    }

    pub(crate) fn failure(&self) -> Option<String> {
        match &*self.0.lock() {
            Lifecycle::Failed(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.0.lock();
        if matches!(*state, Lifecycle::Ready) {
            *state = Lifecycle::Closing;
        }
    }

    pub(crate) fn fail(&self, reason: &str) -> bool {
        let mut state = self.0.lock();
        if matches!(*state, Lifecycle::Failed(_)) {
            return false;
        }
        *state = Lifecycle::Failed(reason.to_owned());
        true
    }
}

/// One client's live `DeviceStream`, shared between the Connect layer and the
/// host thread. Registering a new stream for a client supersedes the old one.
#[derive(Clone, Default, bevy_ecs::prelude::Resource)]
pub(crate) struct DeviceHub {
    inner: Arc<Mutex<HashMap<String, Stream>>>,
}

#[derive(Clone)]
struct Stream {
    subscription: SubscriptionId,
    sender: tokio::sync::mpsc::Sender<w::DeviceEvent>,
}

impl DeviceHub {
    /// Registers `client` as connected and returns the receiver the RPC layer
    /// streams. The previous stream, if any, is closed.
    pub(crate) fn register(
        &self,
        client: &str,
    ) -> (SubscriptionId, tokio::sync::mpsc::Receiver<w::DeviceEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let subscription = SubscriptionId::from(uuid::Uuid::new_v4());
        let mut streams = self.inner.lock();
        streams.insert(
            client.to_owned(),
            Stream {
                subscription,
                sender,
            },
        );
        (subscription, receiver)
    }

    /// Removes only the stream that still owns this generation.
    pub(crate) fn unregister(&self, client: &str, subscription: SubscriptionId) -> bool {
        let mut streams = self.inner.lock();
        if streams
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            streams.remove(client);
            return true;
        }
        false
    }

    pub(crate) fn connected(&self, client: &str) -> bool {
        self.inner.lock().contains_key(client)
    }

    /// The subscription id of the live stream for `client`.
    pub(crate) fn subscription(&self, client: &str) -> Option<SubscriptionId> {
        self.inner
            .lock()
            .get(client)
            .map(|stream| stream.subscription)
    }

    /// Sends `event` to `client`. `false` means no live stream has it.
    pub(crate) fn send(&self, client: &str, event: w::DeviceEvent) -> bool {
        let sender = self
            .inner
            .lock()
            .get(client)
            .map(|stream| stream.sender.clone());
        sender.is_some_and(|sender| sender.try_send(event).is_ok())
    }
}

/// A tool call waiting for a device result.
pub(crate) struct WaitingOperation {
    pub(crate) call: Entity,
    pub(crate) deadline: Instant,
}

/// Device operations whose tool call has not been answered yet.
#[derive(Resource, Default)]
pub(crate) struct Waiting {
    pub(crate) by_operation: HashMap<String, WaitingOperation>,
}

/// The latest perception per companion, injected into its next dynamic tail.
#[derive(Resource, Default)]
pub(crate) struct Perception {
    pub(crate) events: HashMap<String, PerceptionEvent>,
}

#[derive(Clone, Debug)]
pub(crate) struct PerceptionEvent {
    pub(crate) summary: String,
}

/// How long the host waits for a client to answer a device operation.
#[derive(Resource, Clone)]
pub(crate) struct DeviceTimeout(pub(crate) Duration);

#[derive(Resource, Clone)]
pub(crate) struct Options {
    pub(crate) device_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device_timeout: Duration::from_secs(120),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_new_stream_supersedes_the_old_one() {
        let hub = DeviceHub::default();
        let (first, mut receiver) = hub.register("c1");
        let (second, _other) = hub.register("c1");
        assert_ne!(first, second);
        assert!(hub.connected("c1"));
        assert_eq!(hub.subscription("c1"), Some(second));
        assert!(!hub.unregister("c1", first));
        assert!(hub.connected("c1"));
        assert!(hub.unregister("c1", second));
        assert!(!hub.connected("c1"));
        assert!(receiver.try_recv().is_err());
    }
}

/// The host's Tokio runtime, shared with subsystems that need background I/O
/// without creating a runtime or thread of their own.
#[derive(Resource, Clone)]
pub(crate) struct HostRuntime(pub(crate) tokio::runtime::Handle);

/// Tracks the last conversation entry projected into notifications for an agent.
#[derive(Component, Default)]
pub(crate) struct LastProjectedEntry(pub(crate) Option<u64>);

/// One agent per companion, addressed by the companion id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
#[require(LastProjectedEntry)]
pub(crate) struct CompanionAgent {
    pub(crate) companion_id: String,
}

/// One agent per open run, addressed by its run id.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
#[require(LastProjectedEntry)]
pub(crate) struct WorkAgent {
    pub(crate) work_id: String,
    pub(crate) run_id: String,
    pub(crate) companion_id: String,
}

/// Marks an ECS tool entity contributed by an MCP server or plugin extension.
#[derive(Component, Clone, Copy, Debug, Default)]
pub(crate) struct HostExternalTool;

/// Capability tool names currently granted to each active companion.
#[derive(Resource, Clone, Debug, Default)]
pub(crate) struct GrantedCompanionTools(pub(crate) HashMap<String, Vec<String>>);

/// Dispatches a character lifecycle event to installed `Extension::Character` plugins.
#[derive(Event, Clone, Debug)]
pub(crate) struct TriggerCharacterEvent {
    pub(crate) companion_id: String,
    pub(crate) event: ene_plugin::CharacterEvent,
}

/// Speaks character-plugin text via the voice pipeline.
#[derive(Event, Clone, Debug)]
pub(crate) struct SpeakCharacterText {
    pub(crate) companion_id: String,
    pub(crate) text: String,
}
