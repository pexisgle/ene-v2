//! Shared host resources: health, live device streams and perception.
use super::error::{HostError, Result};
use bevy_ecs::prelude::*;
use ene_domain::{ClientId, SubscriptionId, client_view::HostHealth};
use ene_rpc as w;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
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
    inner: Arc<Mutex<DeviceStreams>>,
    wake: rig_ecs::tasks::Wake,
}

#[derive(Default)]
struct DeviceStreams {
    active: HashMap<ClientId, Stream>,
    disconnected: HashSet<ClientId>,
}

struct Stream {
    subscription: SubscriptionId,
    sender: tokio::sync::mpsc::Sender<w::DeviceEvent>,
    body: Option<HashSet<String>>,
}

impl DeviceHub {
    pub(crate) fn new(wake: rig_ecs::tasks::Wake) -> Self {
        Self {
            wake,
            ..Self::default()
        }
    }

    /// Registers `client` as connected and returns the receiver the RPC layer
    /// streams. The previous stream, if any, is closed.
    pub(crate) fn register(
        &self,
        client: &ClientId,
    ) -> (SubscriptionId, tokio::sync::mpsc::Receiver<w::DeviceEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let subscription = SubscriptionId::from(uuid::Uuid::new_v4());
        self.inner.lock().active.insert(
            client.clone(),
            Stream {
                subscription,
                sender,
                body: None,
            },
        );
        (subscription, receiver)
    }

    /// Removes only the stream that still owns this generation.
    pub(crate) fn unregister(&self, client: &ClientId, subscription: SubscriptionId) -> bool {
        let mut streams = self.inner.lock();
        if streams
            .active
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            streams.active.remove(client);
            streams.disconnected.insert(client.clone());
            drop(streams);
            self.wake.wake();
            return true;
        }
        false
    }

    pub(crate) fn connected(&self, client: &str) -> bool {
        self.inner.lock().active.contains_key(client)
    }

    /// Installs renderer readiness only on the generation that reported it.
    pub(crate) fn report_body(
        &self,
        client: &str,
        subscription: &str,
        ready: HashSet<String>,
    ) -> Result<()> {
        let mut streams = self.inner.lock();
        let stream = streams
            .active
            .get_mut(client)
            .filter(|stream| stream.subscription.matches_wire(subscription))
            .ok_or_else(|| HostError::Conflict("the device stream changed".into()))?;
        stream.body = Some(ready);
        Ok(())
    }

    pub(crate) fn capability_available(
        &self,
        client: &str,
        companion: &str,
        spec: Option<&ene_policy::CapabilitySpec>,
    ) -> bool {
        if !spec.is_some_and(|spec| spec.requires_body_availability) {
            return true;
        }
        self.inner
            .lock()
            .active
            .get(client)
            .and_then(|stream| stream.body.as_ref())
            .is_some_and(|ready| ready.contains(companion))
    }

    pub(crate) fn take_disconnected(&self) -> HashSet<ClientId> {
        std::mem::take(&mut self.inner.lock().disconnected)
    }

    /// The subscription id of the live stream for `client`.
    pub(crate) fn subscription(&self, client: &str) -> Option<SubscriptionId> {
        self.inner
            .lock()
            .active
            .get(client)
            .map(|stream| stream.subscription)
    }

    /// Reserves space on a specific stream generation for one dispatch.
    pub(crate) fn reserve(
        &self,
        client: &str,
    ) -> Option<(
        SubscriptionId,
        tokio::sync::mpsc::OwnedPermit<w::DeviceEvent>,
    )> {
        let (subscription, sender) = self
            .inner
            .lock()
            .active
            .get(client)
            .map(|stream| (stream.subscription, stream.sender.clone()))?;
        Some((subscription, sender.try_reserve_owned().ok()?))
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
    pub(crate) approval_key: Option<[u8; 32]>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device_timeout: Duration::from_secs(120),
            approval_key: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_readiness_is_owned_by_one_stream_and_companion() {
        let hub = DeviceHub::default();
        let client = ClientId::parse("c1").unwrap();
        let body = ene_policy::CapabilitySpec {
            requires_body_availability: true,
            ..Default::default()
        };
        let (first, _receiver) = hub.register(&client);
        assert!(!hub.capability_available("c1", "ene", Some(&body)));
        assert!(hub.capability_available("c1", "ene", None));
        hub.report_body("c1", &first.to_string(), HashSet::from(["ene".into()]))
            .unwrap();
        assert!(hub.capability_available("c1", "ene", Some(&body)));
        assert!(!hub.capability_available("c1", "other", Some(&body)));

        let (second, _receiver) = hub.register(&client);
        assert!(
            hub.report_body("c1", &first.to_string(), HashSet::from(["ene".into()]))
                .is_err()
        );
        assert!(!hub.capability_available("c1", "ene", Some(&body)));
        hub.report_body("c1", &second.to_string(), HashSet::from(["other".into()]))
            .unwrap();
        assert!(hub.capability_available("c1", "other", Some(&body)));
        assert!(hub.unregister(&client, second));
        assert!(!hub.capability_available("c1", "other", Some(&body)));
    }

    #[tokio::test]
    async fn a_new_stream_supersedes_the_old_one() {
        let hub = DeviceHub::default();
        let client = ClientId::parse("c1").unwrap();
        let (first, mut receiver) = hub.register(&client);
        let (second, _other) = hub.register(&client);
        assert_ne!(first, second);
        assert!(hub.connected("c1"));
        assert_eq!(hub.subscription("c1"), Some(second));
        assert!(!hub.unregister(&client, first));
        assert!(hub.connected("c1"));
        assert!(hub.unregister(&client, second));
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

/// One Rig agent per open work session, bound to the run it currently serves.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
#[require(LastProjectedEntry)]
pub(crate) struct WorkAgent {
    pub(crate) work_id: String,
    pub(crate) run_id: String,
    pub(crate) companion_id: String,
}

/// The run that admitted a Rig turn, even after its agent serves another run.
#[derive(Component)]
pub(crate) struct WorkTurn(pub(crate) String);

/// Identifies the source of an external tool for permission checks and refreshes.
#[derive(Component, Clone)]
pub(crate) struct HostExternalTool {
    pub(crate) owner: ExternalToolOwner,
    pub(crate) handler: rig_core::serve::ErasedHandler,
}

#[derive(Clone)]
pub(crate) enum ExternalToolOwner {
    Mcp(std::sync::Arc<super::mcp::McpRegistration>),
    Plugin(std::sync::Arc<super::providers::Running>),
}

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
