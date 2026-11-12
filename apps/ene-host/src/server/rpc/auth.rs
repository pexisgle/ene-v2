//! Owner Bearer authentication and loopback Host/Origin checks.
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response as HttpResponse,
};
use connectrpc::ConnectError;
use subtle::ConstantTimeEq;

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

fn loopback_host(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(host, tail)| {
            host == "::1" && (tail.is_empty() || tail.starts_with(':'))
        });
    }
    let host = value.split(':').next().unwrap_or(value);
    matches!(host, "localhost" | "127.0.0.1")
}

fn loopback_origin(value: &str) -> bool {
    let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or(rest);
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(host, tail)| {
            host == "::1" && (tail.is_empty() || tail.starts_with(':'))
        });
    }
    let host = host.split(':').next().unwrap_or(host);
    matches!(host, "localhost" | "127.0.0.1")
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
        for origin in ["https://example.com", "null", "http://0.0.0.0:80"] {
            assert!(!loopback_origin(origin), "{origin}");
        }
    }
}
