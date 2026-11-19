//! Validated plaintext (loopback) and TLS (remote) Bearer configuration for
//! the generated `ene-rpc` clients.
use connectrpc::{
    ConnectError,
    client::{ClientConfig, HttpClient},
};
use http::header::{AUTHORIZATION, HeaderValue};
use url::Url;

fn parse_bearer(token: &str) -> Result<HeaderValue, ConnectError> {
    if token.is_empty() || token.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return Err(ConnectError::invalid_argument(
            "invalid API endpoint or Bearer token",
        ));
    }
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", token))
        .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?;
    bearer.set_sensitive(true);
    Ok(bearer)
}

fn build_client_config(endpoint: &Url, bearer: HeaderValue) -> Result<ClientConfig, ConnectError> {
    Ok(ClientConfig::new(
        endpoint
            .as_str()
            .parse()
            .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?,
    )
    .with_default_header(AUTHORIZATION, bearer))
}

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
    {
        return Err(ConnectError::invalid_argument(
            "invalid API endpoint or Bearer token",
        ));
    }
    let bearer = parse_bearer(token)?;
    // The contract puts the full-duplex voice stream on HTTP/2
    // (docs/api.md §6), and the host's plain listener accepts h2c prior
    // knowledge. `plaintext()` would negotiate HTTP/1.1, which the contract
    // does not name for that path and which a proxy may buffer until the
    // input ends.
    let transport = HttpClient::plaintext_http2_only();
    let config = build_client_config(&endpoint, bearer)?;
    Ok((transport, config))
}

/// Validates a remote host endpoint. Remote connections MUST use HTTPS on `/rpc`,
/// carry no embedded credentials/query/fragment, and must not target unspecified
/// or cloud-metadata addresses.
#[cfg(feature = "client-tls")]
pub fn validate_remote_endpoint(endpoint: &str) -> Result<Url, ConnectError> {
    let parsed = Url::parse(endpoint)
        .map_err(|_| ConnectError::invalid_argument("invalid remote API endpoint"))?;
    if parsed.scheme() != "https"
        || parsed.path() != "/rpc"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ConnectError::invalid_argument(
            "remote API endpoint must be https://<host>/rpc without credentials, query, or fragment",
        ));
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() || ip.is_broadcast() {
                return Err(ConnectError::invalid_argument(
                    "remote API endpoint IP address is not allowed",
                ));
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            let is_link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            if ip.is_unspecified() || is_link_local || ip.is_multicast() {
                return Err(ConnectError::invalid_argument(
                    "remote API endpoint IPv6 address is not allowed",
                ));
            }
        }
        Some(url::Host::Domain(domain)) => {
            let lower = domain.to_ascii_lowercase();
            if lower.is_empty()
                || lower == "metadata.google.internal"
                || lower.ends_with(".metadata.google.internal")
                || lower == "metadata.azure.com"
            {
                return Err(ConnectError::invalid_argument(
                    "remote API endpoint host is not allowed",
                ));
            }
        }
        None => {
            return Err(ConnectError::invalid_argument(
                "remote API endpoint is missing a host",
            ));
        }
    }
    Ok(parsed)
}

/// HTTPS + Bearer client for connecting to an explicitly configured remote
/// `ene-host` ingress (e.g. Tailscale Serve, Cloudflare Tunnel, or TLS reverse proxy).
#[cfg(feature = "client-tls")]
pub fn connect_remote(
    endpoint: &str,
    token: &str,
) -> Result<(HttpClient, ClientConfig), ConnectError> {
    let parsed = validate_remote_endpoint(endpoint)?;
    let bearer = parse_bearer(token)?;
    let mut roots = connectrpc::rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, _) = roots.add_parsable_certificates(native.certs);
    if added == 0 && !native.errors.is_empty() {
        return Err(ConnectError::internal(format!(
            "failed to load system root certificates: {}",
            native.errors[0]
        )));
    }
    let provider = connectrpc::rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| {
            std::sync::Arc::new(connectrpc::rustls::crypto::aws_lc_rs::default_provider())
        });
    let tls = connectrpc::rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| ConnectError::internal(format!("TLS protocol setup failed: {error}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let transport = HttpClient::with_tls(std::sync::Arc::new(tls));
    let config = build_client_config(&parsed, bearer)?;
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

    #[cfg(feature = "client-tls")]
    #[test]
    fn t20_remote_tls_endpoint_validation_enforces_https_and_rejects_plaintext() {
        use super::{connect_remote, validate_remote_endpoint};

        for rejected in [
            "http://ene.example.com/rpc",
            "http://127.0.0.1:8080/rpc",
            "https://ene.example.com/wrong",
            "https://ene.example.com/",
            "https://user@ene.example.com/rpc",
            "https://user:pass@ene.example.com/rpc",
            "https://ene.example.com/rpc?token=secret",
            "https://ene.example.com/rpc#frag",
            "https://169.254.169.254/rpc",
            "https://[fe80::1]/rpc",
            "https://0.0.0.0/rpc",
            "https://[::]/rpc",
            "https://metadata.google.internal/rpc",
            "file:///rpc",
        ] {
            assert!(
                validate_remote_endpoint(rejected).is_err(),
                "must reject {rejected}"
            );
            assert!(
                connect_remote(rejected, "remote-token").is_err(),
                "connect_remote must reject {rejected}"
            );
        }

        for bad_token in ["", "bad\r\nheader", "with space", "日本語"] {
            assert!(connect_remote("https://ene.example.com/rpc", bad_token).is_err());
        }

        for accepted in [
            "https://ene.example.com/rpc",
            "https://ene.tailnet.ts.net:8443/rpc",
        ] {
            let (_, config) = connect_remote(accepted, "super-secret-remote-token").unwrap();
            assert!(!format!("{config:?}").contains("super-secret-remote-token"));
        }
    }
}
