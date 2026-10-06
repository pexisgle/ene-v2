use super::*;

#[derive(Clone)]
pub struct ApiClient(pub(crate) Arc<Inner>);
pub(crate) struct Inner {
    pub(crate) http: reqwest::Client,
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
            || endpoint.path() != "/v1"
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
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Transport)?;
        Ok(Self(Arc::new(Inner {
            http,
            endpoint,
            bearer,
        })))
    }
    pub(crate) fn url(&self, segments: &[&str]) -> Result<Url, Error> {
        let mut url = self.0.endpoint.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| Error::InvalidConnection)?;
        for segment in segments {
            // IDs are opaque; no dot-segment normalization or empty path entries.
            if segment.is_empty() || *segment == "." || *segment == ".." {
                return Err(Error::Protocol);
            }
            path.push(segment);
        }
        drop(path);
        Ok(url)
    }
    pub(crate) async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<&impl Serialize>,
        key: Option<&IdempotencyKey>,
        timeout: Duration,
    ) -> Result<T, Error> {
        let mut request = self
            .0
            .http
            .request(method, url)
            .header(AUTHORIZATION, self.0.bearer.clone())
            .timeout(timeout);
        if let Some(body) = body {
            request = request.json(body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", &key.0);
        }
        let mut response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(Error::Protocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Transport)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Error::Protocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let body = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
            return Err(Error::Api(ApiFailure {
                status: status.as_u16(),
                body,
            }));
        }
        if status == StatusCode::NO_CONTENT {
            serde_json::from_str("null").map_err(|_| Error::Protocol)
        } else {
            serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)
        }
    }
    pub(crate) async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, Error> {
        self.request(Method::GET, url, None::<&()>, None, Duration::from_secs(5))
            .await
    }
    pub(crate) async fn mutate<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&impl Serialize>,
        key: Option<&IdempotencyKey>,
    ) -> Result<T, Error> {
        self.request(method, self.url(path)?, body, key, Duration::from_secs(10))
            .await
    }
}
