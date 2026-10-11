use super::*;

/// Local endpoint and authentication configuration, never sent as an RPC message.
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}

#[derive(Clone)]
pub struct ApiClient(pub(crate) Arc<Inner>);
pub(crate) struct Inner {
    pub(crate) rpc: super::rpc::RpcApi,
    pub(crate) endpoint: Url,
    pub(crate) bearer: HeaderValue,
}
// Credentials and request URLs are intentionally omitted from Debug/errors.
impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient").finish_non_exhaustive()
    }
}
impl ApiClient {
    /// Accepts HTTP only for literal loopback addresses, or verified HTTPS.
    /// Redirects and proxy discovery are disabled, so credentials cannot follow
    /// redirects or be sent through an ambient proxy for local host access.
    pub fn new(connection: &RuntimeConnection) -> Result<Self, Error> {
        let endpoint = Url::parse(&connection.endpoint).map_err(|_| Error::InvalidConnection)?;
        let loopback = match endpoint.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(endpoint.scheme() == "https" || endpoint.scheme() == "http" && loopback)
            || endpoint.host().is_none()
            || endpoint.path() != "/rpc"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || connection.token.is_empty()
            || connection
                .token
                .bytes()
                .any(|b| !(0x21..=0x7e).contains(&b))
        {
            return Err(Error::InvalidConnection);
        }
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", connection.token))
            .map_err(|_| Error::InvalidConnection)?;
        bearer.set_sensitive(true);
        let rpc = super::rpc::RpcApi::new(&endpoint, bearer.clone())?;
        Ok(Self(Arc::new(Inner {
            rpc,
            endpoint,
            bearer,
        })))
    }
}
