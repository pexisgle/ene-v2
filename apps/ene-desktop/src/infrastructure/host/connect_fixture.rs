//! Connect fixtures use the generated contract for both requests and responses.
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use buffa::Message;
use ene_rpc as w;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn encode<T: Message + DeserializeOwned>(value: Value) -> Vec<u8> {
    serde_json::from_value::<T>(value).unwrap().encode_to_vec()
}
fn decode<T: Message + Serialize>(bytes: &[u8]) -> Value {
    serde_json::to_value(T::decode_from_slice(bytes).unwrap()).unwrap()
}
pub fn encode_request(op: &str, value: Value) -> Vec<u8> {
    let value = if value.is_null() { json!({}) } else { value };
    match op {
        "GetSnapshot" => encode::<w::SnapshotRequest>(value),
        "GetSettings" | "ListProviders" | "ReloadProviders" | "ListCompanions"
        | "ReloadPlugins" | "ListPlugins" => encode::<w::Empty>(value),
        "UpdateSettings" => encode::<w::UpdateSettingsRequest>(value),
        "UpdateCompanion" => encode::<w::UpdateCompanionRequest>(value),
        "SendMessage" => encode::<w::SendMessageRequest>(value),
        "GetHistory" => encode::<w::HistoryRequest>(value),
        "GetMessage" => encode::<w::MessageRequest>(value),
        "CreateGrant" => encode::<w::CreateGrantRequest>(value),
        "UpdateGrant" => encode::<w::UpdateGrantRequest>(value),
        "RegisterCredentials" => encode::<w::CredentialRequest>(value),
        "CheckConnection" => encode::<w::CheckRequest>(value),
        "ListModels" => encode::<w::ModelsRequest>(value),
        "GetAvailability" => encode::<w::AvailabilityRequest>(value),
        "EnableProvider" => encode::<w::EnableProviderRequest>(value),
        "ListGrants" => encode::<w::GrantsRequest>(value),
        "Stop" => encode::<w::StopRequest>(value),
        "GetCompanion" | "RevokeGrant" | "RevokeCredentials" => encode::<w::Id>(value),
        _ => panic!("unknown fixture operation {op}"),
    }
}
fn decode_request(op: &str, bytes: &[u8]) -> Value {
    match op {
        "GetSnapshot" => decode::<w::SnapshotRequest>(bytes),
        "GetSettings" | "ListProviders" | "ReloadProviders" | "ListCompanions"
        | "ReloadPlugins" | "ListPlugins" => decode::<w::Empty>(bytes),
        "UpdateSettings" => decode::<w::UpdateSettingsRequest>(bytes),
        "UpdateCompanion" => decode::<w::UpdateCompanionRequest>(bytes),
        "SendMessage" => decode::<w::SendMessageRequest>(bytes),
        "GetHistory" => decode::<w::HistoryRequest>(bytes),
        "GetMessage" => decode::<w::MessageRequest>(bytes),
        "CreateGrant" => decode::<w::CreateGrantRequest>(bytes),
        "UpdateGrant" => decode::<w::UpdateGrantRequest>(bytes),
        "RegisterCredentials" => decode::<w::CredentialRequest>(bytes),
        "CheckConnection" => decode::<w::CheckRequest>(bytes),
        "ListModels" => decode::<w::ModelsRequest>(bytes),
        "GetAvailability" => decode::<w::AvailabilityRequest>(bytes),
        "EnableProvider" => decode::<w::EnableProviderRequest>(bytes),
        "ListGrants" => decode::<w::GrantsRequest>(bytes),
        "Stop" => decode::<w::StopRequest>(bytes),
        "GetCompanion" | "RevokeGrant" | "RevokeCredentials" => decode::<w::Id>(bytes),
        _ => panic!("unknown fixture operation {op}"),
    }
}
pub fn encode_response(op: &str, value: Value) -> Vec<u8> {
    let value = if value.is_null() { json!({}) } else { value };
    match op {
        "GetSnapshot" => encode::<w::Snapshot>(value),
        "GetSettings" | "UpdateSettings" => encode::<w::Settings>(value),
        "GetCompanion" | "UpdateCompanion" => encode::<w::Companion>(value),
        "SendMessage" => encode::<w::MessageAcceptance>(value),
        "RegisterCredentials" => encode::<w::CredentialResult>(value),
        "CreateGrant" | "UpdateGrant" => encode::<w::GrantResult>(value),
        "GetAvailability" => encode::<w::Availability>(value),
        "CheckConnection" => encode::<w::CheckResult>(value),
        "GetMessage" => encode::<w::Message>(value),
        "GetHistory" => encode::<w::History>(value),
        "ListProviders" | "ReloadProviders" | "EnableProvider" => encode::<w::Providers>(value),
        "ListCompanions" => encode::<w::Companions>(value),
        "ListModels" => encode::<w::Models>(value),
        "ListGrants" => encode::<w::Grants>(value),
        "RevokeGrant" | "RevokeCredentials" | "Stop" => encode::<w::Empty>(value),
        _ => panic!("unknown fixture operation {op}"),
    }
}
pub fn decode_response(op: &str, bytes: &[u8]) -> Value {
    match op {
        "GetSnapshot" => decode::<w::Snapshot>(bytes),
        "GetSettings" | "UpdateSettings" => decode::<w::Settings>(bytes),
        "GetCompanion" | "UpdateCompanion" => decode::<w::Companion>(bytes),
        "SendMessage" => decode::<w::MessageAcceptance>(bytes),
        "RegisterCredentials" => decode::<w::CredentialResult>(bytes),
        "CreateGrant" | "UpdateGrant" => decode::<w::GrantResult>(bytes),
        "GetAvailability" => decode::<w::Availability>(bytes),
        "CheckConnection" => decode::<w::CheckResult>(bytes),
        "GetMessage" => decode::<w::Message>(bytes),
        "GetHistory" => decode::<w::History>(bytes),
        "ListProviders" | "ReloadProviders" | "EnableProvider" => decode::<w::Providers>(bytes),
        "ListCompanions" => decode::<w::Companions>(bytes),
        "ListModels" => decode::<w::Models>(bytes),
        "ListGrants" => decode::<w::Grants>(bytes),
        "RevokeGrant" | "RevokeCredentials" | "Stop" => decode::<w::Empty>(bytes),
        _ => panic!("unknown fixture operation {op}"),
    }
}

#[allow(dead_code)]
pub fn router(router: Router) -> Router {
    router.layer(middleware::from_fn(unary))
}
async fn unary(req: Request, next: Next) -> Response {
    let op = req.uri().path().rsplit('/').next().unwrap().to_owned();
    let (mut parts, body) = req.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.unwrap();
    let value = decode_request(&op, &bytes);
    let mut url = url::Url::parse(&format!("http://fixture{}", parts.uri)).unwrap();
    if let Some(id) = value.get("subscriptionId").and_then(Value::as_str) {
        url.query_pairs_mut().append_pair("connection_id", id);
    }
    for key in ["cursor", "limit"] {
        if let Some(value) = value.get(key) {
            url.query_pairs_mut().append_pair(
                key,
                &value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string()),
            );
        }
    }
    parts.uri = format!(
        "{}{}",
        url.path(),
        url.query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default()
    )
    .parse()
    .unwrap();
    parts
        .headers
        .insert("content-type", "application/json".parse().unwrap());
    parts.headers.remove("content-length");
    let response = next
        .run(Request::from_parts(parts, Body::from(value.to_string())))
        .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if value.is_null() && !matches!(op.as_str(), "RevokeGrant" | "RevokeCredentials" | "Stop") {
        return Response::builder()
            .status(status)
            .header("content-type", "application/proto")
            .body(Body::from(bytes))
            .unwrap();
    }
    if !status.is_success() {
        let mut failure: w::ApiFailure = serde_json::from_value(value).unwrap();
        failure.http_status = u32::from(status.as_u16());
        let error = connectrpc::ConnectError::new(
            connectrpc::ErrorCode::FailedPrecondition,
            &failure.error.message,
        )
        .with_detail(connectrpc::ErrorDetail::from_message(
            "ene.host.v1.ApiFailure",
            &failure,
        ));
        return error
            .into_http_response(&axum::http::HeaderMap::new())
            .map(Body::new);
    }
    Response::builder()
        .header("content-type", "application/proto")
        .body(Body::from(encode_response(&op, value)))
        .unwrap()
}
