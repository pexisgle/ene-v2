//! Shared host resources: health, live device streams and perception.
use super::error::{HostError, Result};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// A fatal state means the host must not accept new work.
#[derive(Resource, Default)]
pub(crate) struct HostState {
    pub(crate) failed: Option<String>,
    pub(crate) closing: bool,
}

impl HostState {
    pub(crate) fn ready(&self) -> Result<()> {
        if let Some(reason) = &self.failed {
            return Err(HostError::unavailable(reason.clone()));
        }
        if self.closing {
            return Err(HostError::unavailable("host is stopping"));
        }
        Ok(())
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
    subscription: String,
    sender: tokio::sync::mpsc::Sender<w::DeviceEvent>,
}

impl DeviceHub {
    /// Registers `client` as connected and returns the receiver the RPC layer
    /// streams. The previous stream, if any, is closed.
    pub(crate) fn register(
        &self,
        client: &str,
    ) -> (String, tokio::sync::mpsc::Receiver<w::DeviceEvent>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        let subscription = uuid::Uuid::new_v4().to_string();
        let mut streams = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        streams.insert(
            client.to_owned(),
            Stream {
                subscription: subscription.clone(),
                sender,
            },
        );
        (subscription, receiver)
    }

    pub(crate) fn unregister(&self, client: &str, subscription: &str) {
        let mut streams = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if streams
            .get(client)
            .is_some_and(|stream| stream.subscription == subscription)
        {
            streams.remove(client);
        }
    }

    pub(crate) fn connected(&self, client: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(client)
    }

    /// The subscription id of the live stream for `client`.
    pub(crate) fn subscription(&self, client: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(client)
            .map(|stream| stream.subscription.clone())
    }

    /// Sends `event` to `client`. `false` means no live stream has it.
    pub(crate) fn send(&self, client: &str, event: w::DeviceEvent) -> bool {
        let sender = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
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
        assert_eq!(hub.subscription("c1").as_deref(), Some(second.as_str()));
        hub.unregister("c1", &first);
        assert!(hub.connected("c1"));
        hub.unregister("c1", &second);
        assert!(!hub.connected("c1"));
        assert!(receiver.try_recv().is_err());
    }
}

/// The host's Tokio runtime, shared with subsystems that need background I/O
/// without creating a runtime or thread of their own.
#[derive(Resource, Clone)]
pub(crate) struct HostRuntime(pub(crate) tokio::runtime::Handle);
