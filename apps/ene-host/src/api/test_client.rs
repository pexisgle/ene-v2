//! Wire-protocol fixtures for host integration tests.
use connectrpc::client::{CallOptions, HttpClient};
use ene_rpc::*;
use std::time::Duration;
use tokio::time::timeout;
#[derive(Clone)]
pub struct ApiClient {
    pub host: HostServiceClient<HttpClient>,
    pub devices: DeviceServiceClient<HttpClient>,
    pub plugins: PluginServiceClient<HttpClient>,
}
impl ApiClient {
    pub fn new(endpoint: &str, token: &str) -> Result<Self, connectrpc::ConnectError> {
        let (transport, config) = ene_rpc::connect(endpoint, token)?;
        Ok(Self {
            host: HostServiceClient::new(transport.clone(), config.clone()),
            devices: DeviceServiceClient::new(transport.clone(), config.clone()),
            plugins: PluginServiceClient::new(transport, config),
        })
    }
}
pub fn call_options(seconds: u64) -> CallOptions {
    CallOptions::default().with_timeout(Duration::from_secs(seconds))
}
type RpcStream = connectrpc::client::ServerStream<
    <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
    ene_rpc::__buffa::view::HostEventView<'static>,
>;

pub async fn initial(stream: &mut RpcStream) -> InitialState {
    let first = timeout(Duration::from_secs(5), stream.message::<HostEvent>())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .to_owned_message();
    let Some(host_event::Event::Initial(initial)) = first.event else {
        panic!("initial snapshot missing")
    };
    assert!(!initial.subscription_id.is_empty());
    assert!(initial.snapshot.is_set() && initial.snapshot.cursor.is_set());
    *initial
}

pub fn failure(error: &connectrpc::ConnectError) -> ApiFailure {
    use base64::Engine;
    let detail = error
        .details
        .iter()
        .find(|detail| detail.type_url == "ene.host.v1.ApiFailure")
        .expect("typed API failure");
    let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(detail.value.as_ref().unwrap())
        .unwrap();
    let failure = <ApiFailure as buffa::Message>::decode_from_slice(&bytes).unwrap();
    assert!(failure.error.is_set());
    failure
}
