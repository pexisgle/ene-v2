//! Owner Bearer authentication and strict loopback Host/Origin checks.
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response as HttpResponse,
};
use connectrpc::ConnectError;
use http::uri::Authority;
use subtle::ConstantTimeEq;
use url::Host;

/// What the middleware compares a request against.
#[derive(Clone, Debug)]
pub(super) struct Guard {
    token: String,
    /// Authorities trusted besides loopback, from `ENE_HOST_ALLOWED_HOSTS`.
    /// A tunnel forwards the public name in `Host`, so the operator names it.
    allowed: Vec<ValidatedAuthority>,
}

impl Guard {
    pub(super) fn new(token: String) -> Self {
        let allowed = std::env::var("ENE_HOST_ALLOWED_HOSTS")
            .map(|value| authority_list(&value))
            .unwrap_or_default();
        Self { token, allowed }
    }

    pub(super) fn allowed(&self) -> &[ValidatedAuthority] {
        &self.allowed
    }
}

pub(super) async fn authenticate(
    State(guard): State<Guard>,
    request: Request,
    next: Next,
) -> HttpResponse {
    let allowed = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(guard.token.as_bytes())));
    // A browser Origin must be the host itself or a configured authority; Host
    // is required where the protocol does not carry the authority itself, and
    // anything present must be loopback or explicitly trusted.
    let host_ok = host_allowed(request.version(), request.headers(), &guard.allowed);
    let origin_ok = match request.headers().get(http::header::ORIGIN) {
        Some(header) => header
            .to_str()
            .is_ok_and(|value| origin_allowed(value, &guard.allowed)),
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
fn host_allowed(
    version: http::Version,
    headers: &http::HeaderMap,
    allowed: &[ValidatedAuthority],
) -> bool {
    let mut values = headers.get_all(http::header::HOST).iter();
    let host = values.next();
    if values.next().is_some() {
        return false;
    }
    match host {
        Some(header) => header
            .to_str()
            .ok()
            .and_then(authority)
            .is_some_and(|host| host.is_loopback() || configured(&host, allowed)),
        None => matches!(version, http::Version::HTTP_2 | http::Version::HTTP_3),
    }
}

/// The comma-separated authorities of `ENE_HOST_ALLOWED_HOSTS`, lowercased.
fn authority_list(value: &str) -> Vec<ValidatedAuthority> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| authority(&entry.to_ascii_lowercase()))
        .collect()
}

/// A `Host` or `Origin` authority the operator named: the host must match, and
/// the port too when the entry names one.
fn configured(host: &ValidatedAuthority, allowed: &[ValidatedAuthority]) -> bool {
    allowed.iter().any(|wanted| {
        host.0.host().eq_ignore_ascii_case(wanted.0.host())
            && wanted.0.port().is_none_or(|wanted| {
                host.0
                    .port()
                    .is_some_and(|port| wanted.as_str() == port.as_str())
            })
    })
}

/// HTTP authorities may carry userinfo or an invalid port; ene trusts neither.
#[derive(Clone, Debug)]
pub(super) struct ValidatedAuthority(Authority);

impl ValidatedAuthority {
    fn is_loopback(&self) -> bool {
        matches!(self.0.host(), "[::1]" | "localhost" | "127.0.0.1")
    }
}

impl std::fmt::Display for ValidatedAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

fn authority(value: &str) -> Option<ValidatedAuthority> {
    let parsed: Authority = value.parse().ok()?;
    let suffix = value.strip_prefix(parsed.host())?;
    if value.contains('@')
        || (!suffix.is_empty()
            && !parsed
                .port()
                .is_some_and(|port| suffix.strip_prefix(':') == Some(port.as_str())))
    {
        return None;
    }
    Some(ValidatedAuthority(parsed))
}

/// A `Host` authority: `localhost`, `127.0.0.1` or `[::1]`, with a numeric
/// port. Userinfo, paths and whitespace are never accepted.
#[cfg(test)]
fn loopback_host(value: &str) -> bool {
    authority(value).is_some_and(|host| host.is_loopback())
}

/// An `Origin` header: an http(s) URL whose authority is a literal loopback
/// address, with no userinfo, path, query or fragment.
fn origin_allowed(value: &str, allowed: &[ValidatedAuthority]) -> bool {
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
    let loopback = match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if loopback {
        return true;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // A URL normalises a default port away, so compare with the port the
    // scheme implies: an entry naming :443 still matches an https origin.
    let authority = match url.port_or_known_default() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    self::authority(&authority).is_some_and(|host| configured(&host, allowed))
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
        assert!(host_allowed(
            Version::HTTP_11,
            &hosts(&["127.0.0.1:1234"]),
            &[]
        ));
        assert!(host_allowed(Version::HTTP_11, &hosts(&["[::1]"]), &[]));
        assert!(!host_allowed(Version::HTTP_11, &hosts(&[]), &[]));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["example.com"]),
            &[]
        ));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["localhost", "localhost"]),
            &[]
        ));
        assert!(host_allowed(Version::HTTP_2, &hosts(&[]), &[]));
        assert!(host_allowed(
            Version::HTTP_2,
            &hosts(&["localhost:1234"]),
            &[]
        ));
        assert!(!host_allowed(
            Version::HTTP_2,
            &hosts(&["example.com"]),
            &[]
        ));
        assert!(host_allowed(Version::HTTP_3, &hosts(&[]), &[]));
        let mut unreadable = http::HeaderMap::new();
        unreadable.insert(
            http::header::HOST,
            http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert!(!host_allowed(Version::HTTP_2, &unreadable, &[]));
    }

    #[test]
    fn a_configured_authority_is_trusted_besides_loopback() {
        use http::Version;
        let allowed = authority_list(" ene.example.com , Desk.TS.NET:443 ");
        assert_eq!(
            allowed.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["ene.example.com", "desk.ts.net:443"]
        );
        // The tunnel forwards its public name; the operator named it.
        assert!(host_allowed(
            Version::HTTP_11,
            &hosts(&["ene.example.com"]),
            &allowed
        ));
        assert!(host_allowed(
            Version::HTTP_11,
            &hosts(&["ene.example.com:8443"]),
            &allowed
        ));
        assert!(host_allowed(
            Version::HTTP_11,
            &hosts(&["desk.ts.net:443"]),
            &allowed
        ));
        // A named port is exact, and an unnamed host is not a wildcard.
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["desk.ts.net:8443"]),
            &allowed
        ));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["evil.example.com"]),
            &allowed
        ));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["127.0.0.1.evil.com"]),
            &allowed
        ));
        // Origins use the same list and keep the same shape rules.
        assert!(origin_allowed("https://ene.example.com", &allowed));
        assert!(origin_allowed("https://desk.ts.net:443", &allowed));
        assert!(!origin_allowed("https://evil.example.com", &allowed));
        assert!(!origin_allowed("https://ene.example.com/path", &allowed));
        assert!(!origin_allowed("https://ene.example.com?x=1", &allowed));
        // An entry with no port still matches a default-port origin.
        assert!(origin_allowed("https://ene.example.com", &allowed));
        // Nothing is trusted when the list is empty, whatever the listener.
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["ene.example.com"]),
            &[]
        ));
        assert!(!origin_allowed("https://ene.example.com", &[]));
    }

    #[test]
    fn an_authority_list_drops_entries_that_are_not_authorities() {
        assert!(authority_list("").is_empty());
        assert!(authority_list(" , ").is_empty());
        assert!(authority_list("bad host,host@evil,/path").is_empty());
        assert_eq!(authority_list("Example.COM")[0].to_string(), "example.com");
    }

    #[test]
    fn authority_ports_stay_lexical_and_origin_loopback_stays_broader() {
        use http::Version;
        let allowed = authority_list("Example.COM:0443,[2001:db8::1]:443");
        assert!(host_allowed(
            Version::HTTP_11,
            &hosts(&["EXAMPLE.com:0443"]),
            &allowed
        ));
        assert!(!host_allowed(
            Version::HTTP_11,
            &hosts(&["example.com:443"]),
            &allowed
        ));
        assert!(!origin_allowed("https://example.com:0443", &allowed));
        assert!(origin_allowed("https://[2001:db8::1]", &allowed));
        for value in [
            "localhost:",
            "example.com:",
            "example.com:65536",
            "example.com:no",
            "user@example.com:0443",
            "[::1]evil",
        ] {
            assert!(authority(value).is_none(), "{value}");
            assert!(
                !host_allowed(Version::HTTP_11, &hosts(&[value]), &allowed),
                "{value}"
            );
        }
        for host in ["127.0.0.2", "[0:0:0:0:0:0:0:1]", "LOCALHOST"] {
            assert!(
                !host_allowed(Version::HTTP_11, &hosts(&[host]), &[]),
                "{host}"
            );
            assert!(origin_allowed(&format!("http://{host}"), &[]), "{host}");
        }
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
            "localhost:",
            "localhost:65536",
            "[::1]:",
            "[::1]:notaport",
            "[::1]evil:80",
            "[::1]evil",
            "user@localhost",
            "localhost?query",
            "localhost#fragment",
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
            assert!(origin_allowed(origin, &[]), "{origin}");
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
            assert!(!origin_allowed(origin, &[]), "{origin}");
        }
    }
}
