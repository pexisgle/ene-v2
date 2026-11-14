//! The shared HTTP shape: pooled client, SSRF endpoint guard and bounded body
//! read for the voice engines, embeddings and the model probe.
use futures_util::StreamExt;
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, OnceLock},
    time::Duration,
};

const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;

/// A client per timeout; clones share the connection pool. Redirects stay off
/// so a checked endpoint cannot bounce the request elsewhere.
pub(crate) fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    static CLIENTS: OnceLock<Mutex<HashMap<u64, reqwest::Client>>> = OnceLock::new();
    let millis = u64::try_from(timeout.as_millis())
        .unwrap_or(u64::MAX)
        .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
    let clients = CLIENTS.get_or_init(Default::default);
    let lock = || clients.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some(client) = lock().get(&millis).cloned() {
        return Ok(client);
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(millis))
        .build()
        .map_err(|error| format!("http client: {error}"))?;
    lock().entry(millis).or_insert(client.clone());
    Ok(client)
}

/// Private, loopback, link-local, unspecified, multicast or broadcast address;
/// IPv4-mapped IPv6 is judged as the IPv4 address it carries.
pub(crate) fn blocked_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
                || address.is_multicast()
                || address.is_documentation()
        }
        IpAddr::V6(address) => {
            if let Some(mapped) = address.to_ipv4_mapped() {
                return blocked_ip(IpAddr::V4(mapped));
            }
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
        }
    }
}

/// Whether `endpoint` is an http(s) URL on the loopback interface.
pub(crate) fn loopback_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https") && url.host().is_some_and(|host| loopback(&host))
    })
}

fn loopback(host: &url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => {
            address.is_loopback()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
    }
}

/// The endpoint guard shared by every host HTTP call. Loopback may use plain
/// HTTP; any other plain-HTTP host is refused even without a credential, and a
/// non-public resolved address is refused. Errors never carry URL or secret.
pub(crate) async fn guard_endpoint(url: &str, what: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| format!("the {what} endpoint is not a URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("the {what} endpoint must be http(s)"));
    }
    let Some(host) = parsed.host() else {
        return Err(format!("the {what} endpoint has no host"));
    };
    if loopback(&host) {
        return Ok(());
    }
    if parsed.scheme() == "http" {
        return Err(format!(
            "the {what} endpoint must use https outside loopback"
        ));
    }
    match host {
        url::Host::Ipv4(address) => {
            if blocked_ip(IpAddr::V4(address)) {
                return Err(format!("the {what} endpoint is not a public address"));
            }
        }
        url::Host::Ipv6(address) => {
            if blocked_ip(IpAddr::V6(address)) {
                return Err(format!("the {what} endpoint is not a public address"));
            }
        }
        url::Host::Domain(domain) => {
            let port = parsed
                .port_or_known_default()
                .ok_or_else(|| format!("the {what} endpoint has no port"))?;
            let addresses: Vec<_> = tokio::net::lookup_host((domain, port))
                .await
                .map_err(|error| format!("the {what} endpoint address lookup: {error}"))?
                .collect();
            if addresses.is_empty() {
                return Err(format!("the {what} endpoint did not resolve"));
            }
            if addresses.iter().any(|address| blocked_ip(address.ip())) {
                return Err(format!(
                    "the {what} endpoint resolves to a non-public address"
                ));
            }
        }
    }
    Ok(())
}

/// Reads at most `max` bytes: a declared oversize is refused before the body,
/// and a chunked answer is cut at the cap while it streams.
pub(crate) async fn read_body(
    response: reqwest::Response,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(too_large(what, max));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("the {what} endpoint body: {error}"))?;
        if chunk.len() > max - body.len() {
            return Err(too_large(what, max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn too_large(what: &str, max: usize) -> String {
    format!("the {what} endpoint answered more data than the host reads ({max} bytes)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn private_and_mapped_addresses_are_blocked() {
        for address in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.4",
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(blocked_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "2606:4700:4700::1111"] {
            assert!(!blocked_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[tokio::test]
    async fn the_guard_allows_loopback_and_rejects_plain_remote_http() {
        for allowed in [
            "http://127.0.0.1:5002",
            "http://[::1]:5002",
            "http://localhost:5002",
            "https://127.0.0.1",
            "https://[::ffff:127.0.0.1]",
        ] {
            assert!(guard_endpoint(allowed, "test").await.is_ok(), "{allowed}");
            assert!(loopback_endpoint(allowed), "{allowed}");
        }
        for rejected in [
            "http://example.com/v1",
            "http://10.0.0.1/v1",
            "https://10.0.0.1",
            "https://169.254.169.254",
            "https://[fe80::1]",
            "https://[::ffff:10.0.0.1]",
            "ftp://127.0.0.1",
        ] {
            assert!(
                guard_endpoint(rejected, "test").await.is_err(),
                "{rejected}"
            );
        }
    }

    async fn serve(response: Vec<u8>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 1_024];
                let _ = socket.read(&mut request).await;
                let _ = socket.write_all(&response).await;
            }
        });
        address
    }

    #[tokio::test]
    async fn bodies_are_refused_at_the_cap_declared_or_streamed() {
        let mut chunked = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
        chunked.extend_from_slice(&[b'x'; 128]);
        let declared = b"HTTP/1.1 200 OK\r\nContent-Length: 128\r\n\r\n".to_vec();
        for response in [declared, chunked] {
            let address = serve(response).await;
            let response = client(Duration::from_secs(5))
                .unwrap()
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap();
            let error = read_body(response, 64, "test").await.unwrap_err();
            assert!(error.contains("more data"), "{error}");
        }
    }
}
