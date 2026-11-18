//! Validated plaintext and Bearer configuration for the generated `ene-rpc` clients.
use connectrpc::{
    ConnectError,
    client::{ClientConfig, HttpClient},
};
use http::header::{AUTHORIZATION, HeaderValue};
use url::Url;

/// HTTP on a literal loopback address, and nothing else. Credentials never
/// follow redirects.
pub fn connect(endpoint: &str, token: &str) -> Result<(HttpClient, ClientConfig), ConnectError> {
    let endpoint = Url::parse(endpoint)
        .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?;
    let loopback = match endpoint.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if endpoint.scheme() != "http"
        || !loopback
        || endpoint.path() != "/rpc"
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || token.is_empty()
        || token.bytes().any(|b| !(0x21..=0x7e).contains(&b))
    {
        return Err(ConnectError::invalid_argument(
            "invalid API endpoint or Bearer token",
        ));
    }
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", token))
        .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?;
    bearer.set_sensitive(true);
    // The contract puts the full-duplex voice stream on HTTP/2
    // (docs/api.md §6), and the host's plain listener accepts h2c prior
    // knowledge. `plaintext()` would negotiate HTTP/1.1, which the contract
    // does not name for that path and which a proxy may buffer until the
    // input ends.
    let transport = HttpClient::plaintext_http2_only();
    let config = ClientConfig::new(
        endpoint
            .as_str()
            .parse()
            .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?,
    )
    .with_default_header(AUTHORIZATION, bearer);
    Ok((transport, config))
}

#[cfg(test)]
mod tests {
    use super::connect;

    #[test]
    fn endpoints_and_tokens_reject_unsafe_connections() {
        for endpoint in [
            "https://127.0.0.1:1234/rpc",
            "http://remote/rpc",
            "http://localhost/rpc",
            "http://127.0.0.1/rpc?token=secret",
            "https://user@remote/rpc",
            "https://remote/rpc#secret",
            "file:///rpc",
            "http://127.0.0.1/wrong",
            "http://127.0.0.1/v1",
            "http://127.0.0.1/rpc#secret",
            "http://user@127.0.0.1/rpc",
            "http://user:secret@127.0.0.1/rpc",
            "http://127.0.0.1:65536/rpc",
            "http://127.0.0.1:invalid/rpc",
            "http://[::1]evil:80/rpc",
            "http://[::1]evil/rpc",
        ] {
            assert!(connect(endpoint, "credential").is_err());
        }
        for token in ["", "secret\r\nother", "not a token", "日本語"] {
            assert!(connect("http://127.0.0.1:1234/rpc", token).is_err());
        }
        for endpoint in ["http://127.0.0.1:1234/rpc", "http://[::1]:1234/rpc"] {
            let (_, config) = connect(endpoint, "credential").unwrap();
            assert!(!format!("{config:?}").contains("credential"));
        }
    }
}
