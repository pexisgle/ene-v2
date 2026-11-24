//! The HTTP calls shared by every `HttpEngine` protocol: send with the bearer
//! credential, reject non-success statuses, and read the body under a size cap.

use crate::error::{HttpResult, VoiceHttpError};

pub(crate) async fn read_response_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    label: &str,
) -> HttpResult<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) =
        response
            .chunk()
            .await
            .map_err(|error| VoiceHttpError::ResponseRead {
                label: label.to_owned(),
                error: error.without_url().to_string(),
            })?
    {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(VoiceHttpError::ResponseTooLarge(label.to_owned()));
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
    let response = call
        .send()
        .await
        .map_err(|error| VoiceHttpError::EndpointUnreachable {
            label: label.to_owned(),
            error: error.to_string(),
        })?;
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(VoiceHttpError::EndpointStatus {
            label: label.to_owned(),
            status: response.status().as_u16(),
        })
    }
}
