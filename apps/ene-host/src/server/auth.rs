//! Owner Bearer authentication and strict loopback Host/Origin checks.
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response as HttpResponse,
};
use connectrpc::ConnectError;
use subtle::ConstantTimeEq;
use url::Host;

pub(super) async fn authenticate(
    State(token): State<String>,
    request: Request,
    next: Next,
) -> HttpResponse {
    let allowed = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(token.as_bytes())));
    // A browser Origin must be the host itself; an absent Host is left to the
    // transport (HTTP/2 carries it as :authority), but anything present must
    // be a literal loopback authority.
    let host_ok = request
        .headers()
        .get(http::header::HOST)
        .and_then(|header| header.to_str().ok())
        .is_none_or(loopback_host);
    let origin_ok = request
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|header| header.to_str().ok())
        .is_none_or(loopback_origin);
    if !allowed || !host_ok || !origin_ok {
        return ConnectError::unauthenticated("owner credential required")
            .into_http_response(request.headers())
            .map(axum::body::Body::new);
    }
    next.run(request).await
}

/// A `Host` authority: `localhost`, `127.0.0.1` or `[::1]`, with a numeric
/// port. Userinfo, paths and whitespace are never accepted.
fn loopback_host(value: &str) -> bool {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'@' | b'/' | b'?' | b'#'))
    {
        return false;
    }
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return false;
        };
        let host = &value[..end + 2];
        let tail = &rest[end + 1..];
        let port = if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':').unwrap_or(""))
        };
        (host, port)
    } else {
        match value.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (value, None),
        }
    };
    if let Some(port) = port
        && (port.is_empty() || port.parse::<u16>().is_err())
    {
        return false;
    }
    host == "[::1]" || matches!(host, "localhost" | "127.0.0.1")
}

/// An `Origin` header: an http(s) URL whose authority is a literal loopback
/// address, with no userinfo, path, query or fragment.
fn loopback_origin(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return false;
    }
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_authorities_and_origins_are_trusted() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:1234",
            "localhost",
            "localhost:1234",
            "[::1]",
            "[::1]:1234",
        ] {
            assert!(loopback_host(host), "{host}");
        }
        for host in [
            "example.com",
            "example.com:80",
            "0.0.0.0:80",
            "[2001:db8::1]",
            "localhost@evil.com",
            "127.0.0.1@evil.com",
            "localhost:notaport",
            "localhost/evil",
            "127.0.0.1.evil.com",
        ] {
            assert!(!loopback_host(host), "{host}");
        }
        for origin in [
            "http://127.0.0.1:1234",
            "http://localhost:1234",
            "https://[::1]:1234",
        ] {
            assert!(loopback_origin(origin), "{origin}");
        }
        for origin in [
            "https://example.com",
            "null",
            "http://0.0.0.0:80",
            "http://localhost@evil.com",
            "http://localhost:@evil.com",
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1/path",
            "http://127.0.0.1?x=1",
            "http://user:pass@127.0.0.1",
            "ftp://127.0.0.1",
        ] {
            assert!(!loopback_origin(origin), "{origin}");
        }
    }
}
