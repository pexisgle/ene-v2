//! The HTTP calls shared by every `HttpEngine` protocol: send with the bearer
//! credential, reject non-success statuses, and read the body under a size cap.

use crate::error::{HttpResult, VoiceHttpError};

pub(crate) async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> HttpResult<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        VoiceHttpError::Transport(format!(
            "the {label} response failed: {}",
            error.without_url()
        ))
    })? {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(VoiceHttpError::LimitExceeded(format!(
                "the {label} response exceeded the host limit"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

pub(crate) async fn post(
    call: reqwest::RequestBuilder,
    secret: Option<&str>,
    label: &str,
) -> HttpResult<reqwest::Response> {
    let call = match secret {
        Some(secret) => call.bearer_auth(secret),
        None => call,
    };
    let response = call.send().await.map_err(|error| {
        VoiceHttpError::Transport(format!(
            "the {label} endpoint is unreachable: {}",
            error.without_url()
        ))
    })?;
    let status = response.status();
    if status.is_success() {
        Ok(response)
    } else {
        Err(VoiceHttpError::Protocol(format!(
            "the {label} endpoint answered {}",
            status.as_u16()
        )))
    }
}
