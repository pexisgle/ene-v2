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
    // A browser Origin must be the host itself; Host is required where the
    // protocol does not carry the authority itself, and anything present must
    // be a literal loopback authority.
    let host_ok = host_allowed(request.version(), request.headers());
    let origin_ok = match request.headers().get(http::header::ORIGIN) {
        Some(header) => header.to_str().is_ok_and(loopback_origin),
        None => true,
    };
    if !allowed || !host_ok || !origin_ok {
        return ConnectError::unauthenticated("owner credential required")
            .into_http_response(request.headers())
            .map(axum::body::Body::new);
    }
    next.run(request).await
}

/// The `Host` authority this transport carries. HTTP/2 and HTTP/3 send the
/// authority in the protocol (`:authority`), so an absent `Host` is accepted
/// there; every other version, HTTP/1.1 included, must name one. Whatever is
/// present is still checked, and a repeated `Host` is never trusted.
fn host_allowed(version: http::Version, headers: &http::HeaderMap) -> bool {
    let mut values = headers.get_all(http::header::HOST).iter();
    let host = values.next();
    if values.next().is_some() {
        return false;
    }
    match host {
        Some(header) => header.to_str().is_ok_and(loopback_host),
        None => matches!(version, http::Version::HTTP_2 | http::Version::HTTP_3),
    }
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

    fn hosts(values: &[&str]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for value in values {
            headers.append(
                http::header::HOST,
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn http_1_1_requires_a_host_and_http_2_carries_the_authority_itself() {
        use http::Version;
        assert!(host_allowed(Version::HTTP_11, &hosts(&["127.0.0.1:1234"])));
        assert!(host_allowed(Version::HTTP_11, &hosts(&["[::1]"])));
        assert!(!host_allowed(Version::HTTP_11, &hosts(&[])));
        assert!(!host_allowed(Version::HTTP_11, &hosts(&["example.com"])));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["localhost", "localhost"])
        ));
        assert!(host_allowed(Version::HTTP_2, &hosts(&[])));
        assert!(host_allowed(Version::HTTP_2, &hosts(&["localhost:1234"])));
        assert!(!host_allowed(Version::HTTP_2, &hosts(&["example.com"])));
        assert!(host_allowed(Version::HTTP_3, &hosts(&[])));
        let mut unreadable = http::HeaderMap::new();
        unreadable.insert(
            http::header::HOST,
            http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert!(!host_allowed(Version::HTTP_2, &unreadable));
    }

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
