//! Header ownership at the broker boundary.
use crate::limits::MAX_HTTP_HEADERS;
use ene_plugin::Failure;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::BTreeMap;

fn is_host_owned(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "host" | "content-length"
    )
}

pub(super) fn request_headers(input: &BTreeMap<String, String>) -> Result<HeaderMap, Failure> {
    let mut headers = HeaderMap::new();
    for (name, value) in input {
        // Only the host can choose the authenticated identity and destination.
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| Failure::new("invalid_header"))?;
        if is_host_owned(&name) {
            return Err(Failure::new("reserved_header"));
        }
        let value = HeaderValue::from_str(value).map_err(|_| Failure::new("invalid_header"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

/// The reply headers a plugin may see, bounded.
pub(super) fn copy_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut copied = BTreeMap::new();
    let mut total = 0usize;
    for (name, value) in headers {
        // The host's own credential and transport details stay out of the
        // plugin's view.
        if is_host_owned(name) || matches!(name.as_str(), "set-cookie" | "transfer-encoding") {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        total += name.as_str().len() + value.len();
        if total > MAX_HTTP_HEADERS {
            break;
        }
        copied.insert(name.as_str().to_owned(), value.to_owned());
    }
    copied
}
