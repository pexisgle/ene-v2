use super::*;
/// Dynamic provider options and metadata require a self-describing codec.
/// MessagePack encodes these Serde values without a text framing protocol.
pub type Codec = remoc::codec::MessagePack;
pub type Client = ProviderClient<Codec>;
pub type BrokerClient = HttpBrokerClient<Codec>;
pub type Sender<T> = remoc::rch::mpsc::Sender<T, Codec>;

#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum ProviderError {
    #[error("provider failed")]
    Failed,
    #[error("invalid input")]
    InvalidInput,
    #[error("permission denied")]
    PermissionDenied,
    #[error("provider unavailable")]
    Unavailable,
    #[error(transparent)]
    Call(#[from] remoc::rtc::CallError),
}

/// The broker capability is scoped by the host to this invocation.
/// Dropping a call cancels it; dropping the client stops the provider server.
#[remoc::rtc::remote]
pub trait Provider {
    async fn capabilities(&self) -> Result<Capabilities, ProviderError>;
    async fn list_models(
        &self,
        endpoint: String,
        options: Value,
        broker: BrokerClient,
    ) -> Result<Vec<Model>, ProviderError>;
    /// Text arrives in order on `deltas`; the RPC result is the finish reason.
    async fn generate(
        &self,
        request: GenerationRequest,
        broker: BrokerClient,
        deltas: Sender<String>,
    ) -> Result<String, ProviderError>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HttpResponsePart {
    Head {
        status: u16,
        headers: BTreeMap<String, String>,
    },
    Data(Vec<u8>),
}

/// Drain the response channel concurrently with the call to apply backpressure.
/// The RPC result reports completion or failure of the HTTP exchange.
#[remoc::rtc::remote]
pub trait HttpBroker {
    async fn request(
        &self,
        request: HttpRequest,
        response: Sender<HttpResponsePart>,
    ) -> Result<(), ProviderError>;
}
