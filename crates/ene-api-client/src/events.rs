use super::*;
use tokio::time::timeout;

pub enum ServerEvent {
    Notification(Notification),
    ResyncRequired,
}
/// Identity and cursor of the initial Connect snapshot.
pub struct SubscriptionInfo {
    pub connection_id: String,
    pub cursor: Cursor,
}
/// Bound to one live subscription. Do not reuse its ID after closing/reconnecting.
pub struct EventStream {
    rpc: Option<RpcStream>,
    initial: Option<Snapshot>,
    ready: SubscriptionInfo,
    closed: bool,
}
type RpcStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    ene_api_types::rpc::__buffa::view::HostEventView<'static>,
>;

impl ApiClient {
    pub async fn subscribe(&self) -> Result<EventStream, Error> {
        let rpc = &self.0.rpc;
        let mut stream = timeout(
            Duration::from_secs(5),
            rpc.host.subscribe(ene_api_types::rpc::Empty::default()),
        )
        .await
        .map_err(|_| Error::Transport)?
        .map_err(super::rpc::rpc_error)?;
        let first = timeout(
            Duration::from_secs(5),
            stream.message::<ene_api_types::rpc::HostEvent>(),
        )
        .await
        .map_err(|_| Error::Transport)?
        .map_err(super::rpc::rpc_error)?
        .ok_or(Error::Protocol)?
        .to_owned_message();
        let Some(ene_api_types::rpc::host_event::Event::Initial(initial)) = first.event else {
            return Err(Error::Protocol);
        };
        if initial.subscription_id.is_empty() {
            return Err(Error::Protocol);
        }
        let snapshot: Snapshot =
            super::rpc::decode(initial.snapshot.into_option().ok_or(Error::Protocol)?)?;
        let ready = SubscriptionInfo {
            connection_id: initial.subscription_id,
            cursor: snapshot.cursor.clone(),
        };
        Ok(EventStream {
            rpc: Some(stream),
            initial: Some(snapshot),
            ready,
            closed: false,
        })
    }
}

impl EventStream {
    /// Take the atomic initial snapshot supplied by a Connect subscription.
    pub fn take_initial_snapshot(&mut self) -> Option<Snapshot> {
        self.initial.take()
    }
    pub fn ready(&self) -> &SubscriptionInfo {
        &self.ready
    }
    pub async fn next(&mut self) -> Result<Option<ServerEvent>, Error> {
        if self.closed {
            return Ok(None);
        }
        if let Some(stream) = &mut self.rpc {
            let Some(message) = stream
                .message::<ene_api_types::rpc::HostEvent>()
                .await
                .map_err(super::rpc::rpc_error)?
            else {
                self.closed = true;
                return Ok(None);
            };
            return match message.to_owned_message().event {
                Some(ene_api_types::rpc::host_event::Event::Notification(event)) => {
                    Ok(Some(ServerEvent::Notification(super::rpc::decode(*event)?)))
                }
                Some(ene_api_types::rpc::host_event::Event::ResyncRequired(_)) => {
                    self.closed = true;
                    self.rpc = None;
                    Ok(Some(ServerEvent::ResyncRequired))
                }
                _ => Err(Error::Protocol),
            };
        }
        Ok(None)
    }
    /// Dropping the response body closes the Connect subscription.
    pub async fn close(mut self) -> Result<(), Error> {
        self.rpc.take();
        Ok(())
    }
}
