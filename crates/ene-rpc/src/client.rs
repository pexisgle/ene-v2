//! Validated TLS and Bearer configuration for the generated `ene-rpc` clients.
use connectrpc::{
    ConnectError,
    client::{ClientConfig, HttpClient},
};
use http::header::{AUTHORIZATION, HeaderValue};
use std::{sync::Arc, time::Duration};
use url::Url;

/// HTTP is restricted to literal loopback addresses. Credentials never follow redirects.
pub fn connect(endpoint: &str, token: &str) -> Result<(HttpClient, ClientConfig), ConnectError> {
    let endpoint = Url::parse(endpoint)
        .map_err(|_| ConnectError::invalid_argument("invalid API endpoint or Bearer token"))?;
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
    let builder = HttpClient::builder().connect_timeout(Duration::from_secs(5));
    let transport = if endpoint.scheme() == "https" {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| ConnectError::unavailable("TLS initialization failed"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        builder.with_tls(Arc::new(tls))
    } else {
        builder.plaintext()
    };
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
            "http://remote/rpc",
            "http://localhost/rpc",
            "http://127.0.0.1/rpc?token=secret",
            "https://user@remote/rpc",
            "https://remote/rpc#secret",
            "file:///rpc",
            "http://127.0.0.1/wrong",
            "http://127.0.0.1/v1",
        ] {
            assert!(connect(endpoint, "credential").is_err());
        }
        for token in ["", "secret\r\nother", "not a token", "日本語"] {
            assert!(connect("http://127.0.0.1:1234/rpc", token).is_err());
        }
        for endpoint in [
            "http://127.0.0.1:1234/rpc",
            "http://[::1]:1234/rpc",
            "https://remote.example/rpc",
        ] {
            let (_, config) = connect(endpoint, "credential").unwrap();
            assert!(!format!("{config:?}").contains("credential"));
        }
    }
}
