//! Unary Connect fixtures sharing the application's storage DTOs.
//! Requests and responses on the wire are generated protobuf messages.
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use buffa::Message;
use ene_api_types::{host as h, provider as p, rpc as w, rpc_conversion::RpcConvert};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

fn parse<T: DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}
fn value<T: Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap()
}
fn wire<T: RpcConvert + DeserializeOwned>(v: Value) -> Vec<u8>
where
    T::Wire: Message,
{
    parse::<T>(v).into_wire().unwrap().encode_to_vec()
}
fn read<T: RpcConvert + Serialize>(b: &[u8]) -> Value
where
    T::Wire: Message,
{
    value(T::from_wire(T::Wire::decode_from_slice(b).unwrap()).unwrap())
}
fn encode<T: Message + DeserializeOwned>(v: Value) -> Vec<u8> {
    parse::<T>(v).encode_to_vec()
}
fn decode<T: Message + Serialize>(b: &[u8]) -> Value {
    value(T::decode_from_slice(b).unwrap())
}

pub fn encode_request(op: &str, mut v: Value) -> Vec<u8> {
    if v.is_null() {
        v = json!({});
    }
    match op {
        "GetSnapshot" => encode::<w::SnapshotRequest>(v),
        "GetSettings" | "ListProviders" | "ReloadProviders" | "ListCompanions" => {
            encode::<w::Empty>(v)
        }
        "UpdateSettings" => {
            for c in v["connections"].as_array_mut().unwrap() {
                c["options"] = json!({"json":c["options"].to_string()});
            }
            encode::<w::UpdateSettingsRequest>(v)
        }
        "UpdateCompanion" => {
            let assignments = v.as_object_mut().unwrap().remove("model_assignments");
            if let Some(Value::Object(assignments)) = assignments {
                for slot in ["companion", "task"] {
                    if let Some(a) = assignments.get(slot) {
                        v[format!("{slot}_assignment")] = if a.is_null() {
                            json!({"clear":{}})
                        } else {
                            json!({"set":a})
                        };
                    }
                }
            }
            encode::<w::UpdateCompanionRequest>(v)
        }
        "SendMessage" => {
            if let Some(resources) = v["resources"].as_array_mut() {
                for r in resources {
                    *r = json!({"json":r.to_string()});
                }
            }
            encode::<w::SendMessageRequest>(v)
        }
        "GetHistory" => encode::<w::HistoryRequest>(v),
        "GetMessage" => encode::<w::MessageRequest>(v),
        "CreateGrant" => encode::<w::CreateGrantRequest>(v),
        "UpdateGrant" => {
            for k in ["destinations", "data_kinds"] {
                if v[k].is_array() {
                    v[k] = json!({"values":v[k]});
                }
            }
            encode::<w::UpdateGrantRequest>(v)
        }
        "RegisterCredentials" => encode::<w::CredentialRequest>(v),
        "CheckConnection" => encode::<w::CheckRequest>(v),
        "ListModels" => encode::<w::ModelsRequest>(v),
        "GetAvailability" => encode::<w::AvailabilityRequest>(v),
        "EnableProvider" => encode::<w::EnableProviderRequest>(v),
        "ListGrants" => encode::<w::GrantsRequest>(v),
        "Stop" => encode::<w::StopRequest>(v),
        "GetCompanion" | "RevokeGrant" | "RevokeCredentials" => encode::<w::Id>(v),
        _ => panic!("unknown fixture operation {op}"),
    }
}
fn decode_request(op: &str, b: &[u8]) -> Value {
    let mut v = match op {
        "GetSnapshot" => decode::<w::SnapshotRequest>(b),
        "GetSettings" | "ListProviders" => decode::<w::Empty>(b),
        "ListGrants" => decode::<w::GrantsRequest>(b),
        "UpdateSettings" => decode::<w::UpdateSettingsRequest>(b),
        "UpdateCompanion" => decode::<w::UpdateCompanionRequest>(b),
        "SendMessage" => decode::<w::SendMessageRequest>(b),
        "GetHistory" => decode::<w::HistoryRequest>(b),
        "CreateGrant" => decode::<w::CreateGrantRequest>(b),
        "UpdateGrant" => decode::<w::UpdateGrantRequest>(b),
        "RegisterCredentials" => decode::<w::CredentialRequest>(b),
        "CheckConnection" => decode::<w::CheckRequest>(b),
        "GetAvailability" => decode::<w::AvailabilityRequest>(b),
        "RevokeGrant" | "RevokeCredentials" => decode::<w::Id>(b),
        _ => panic!("unknown fixture operation {op}"),
    };
    if op == "UpdateSettings" {
        for c in v["connections"].as_array_mut().unwrap() {
            c["options"] = serde_json::from_str(c["options"]["json"].as_str().unwrap()).unwrap();
        }
    }
    if op == "UpdateCompanion" {
        for slot in ["companion", "task"] {
            if let Some(a) = v
                .as_object_mut()
                .unwrap()
                .remove(&format!("{slot}Assignment"))
            {
                v["model_assignments"][slot] = a.get("set").cloned().unwrap_or(Value::Null);
            }
        }
    }
    if op == "UpdateGrant" {
        for k in ["destinations", "dataKinds"] {
            if !v[k].is_null() {
                v[k] = v[k]["values"].clone();
            }
        }
    }
    // Generated protobuf JSON uses camelCase and string-valued 64-bit integers.
    fn dto(v: Value) -> Value {
        match v {
            Value::Object(o) => Value::Object(
                o.into_iter()
                    .map(|(k, v)| {
                        let mut s = String::new();
                        for c in k.chars() {
                            if c.is_uppercase() {
                                s.push('_');
                                s.extend(c.to_lowercase());
                            } else {
                                s.push(c);
                            }
                        }
                        (s, dto(v))
                    })
                    .collect(),
            ),
            Value::Array(a) => Value::Array(a.into_iter().map(dto).collect()),
            x => x,
        }
    }
    let mut v = dto(v);
    for path in ["/revision", "/consent/settings_revision"] {
        if let Some(r) = v.pointer_mut(path)
            && let Some(s) = r.as_str()
        {
            *r = json!(s.parse::<u64>().unwrap());
        }
    }
    for k in ["id", "request_key", "companion_id"] {
        if (k == "id"
            && matches!(
                op,
                "UpdateCompanion" | "RegisterCredentials" | "UpdateGrant" | "CheckConnection"
            ))
            || k == "request_key"
            || (k == "companion_id" && op == "SendMessage")
        {
            v.as_object_mut().unwrap().remove(k);
        }
    }
    v
}
pub fn encode_response(op: &str, v: Value) -> Vec<u8> {
    match op {
        "GetSnapshot" => wire::<h::Snapshot>(v),
        "GetSettings" | "UpdateSettings" => wire::<h::Settings>(v),
        "GetCompanion" | "UpdateCompanion" => wire::<h::Companion>(v),
        "SendMessage" => wire::<h::MessageAcceptance>(v),
        "RegisterCredentials" => wire::<h::CredentialResult>(v),
        "CreateGrant" | "UpdateGrant" => wire::<h::GrantResult>(v),
        "GetAvailability" => wire::<h::ConnectionAvailability>(v),
        "CheckConnection" => wire::<h::CheckResult>(v),
        "GetMessage" => wire::<h::Message>(v),
        "GetHistory" => {
            let page: h::Page<h::Message> = parse(v);
            w::History {
                items: page
                    .items
                    .into_iter()
                    .map(|m| m.into_wire().unwrap())
                    .collect(),
                next_cursor: page.next_cursor,
                ..Default::default()
            }
            .encode_to_vec()
        }
        "ListProviders" | "ReloadProviders" | "EnableProvider" => w::Providers {
            items: parse::<Vec<p::ProviderInfo>>(v)
                .into_iter()
                .map(|m| m.into_wire().unwrap())
                .collect(),
            ..Default::default()
        }
        .encode_to_vec(),
        "ListGrants" => w::Grants {
            items: parse::<Vec<h::DataTransferGrant>>(v)
                .into_iter()
                .map(|m| m.into_wire().unwrap())
                .collect(),
            ..Default::default()
        }
        .encode_to_vec(),
        "RevokeGrant" | "RevokeCredentials" | "Stop" => w::Empty::default().encode_to_vec(),
        _ => panic!("unknown fixture operation {op}"),
    }
}
pub fn decode_response(op: &str, b: &[u8]) -> Value {
    match op {
        "GetSnapshot" => read::<h::Snapshot>(b),
        "GetSettings" | "UpdateSettings" => read::<h::Settings>(b),
        "GetCompanion" | "UpdateCompanion" => read::<h::Companion>(b),
        "SendMessage" => read::<h::MessageAcceptance>(b),
        "RegisterCredentials" => read::<h::CredentialResult>(b),
        "CreateGrant" | "UpdateGrant" => read::<h::GrantResult>(b),
        "GetAvailability" => read::<h::ConnectionAvailability>(b),
        "CheckConnection" => read::<h::CheckResult>(b),
        "GetMessage" => read::<h::Message>(b),
        "ListGrants" => value(
            w::Grants::decode_from_slice(b)
                .unwrap()
                .items
                .into_iter()
                .map(|m| h::DataTransferGrant::from_wire(m).unwrap())
                .collect::<Vec<_>>(),
        ),
        "RevokeGrant" | "RevokeCredentials" | "Stop" => Value::Null,
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
    let bytes = to_bytes(body, 16 * 1024 * 1024).await.unwrap();
    let v = decode_request(&op, &bytes);
    let mut url = url::Url::parse(&format!("http://fixture{}", parts.uri)).unwrap();
    if let Some(id) = v.get("subscription_id").and_then(Value::as_str) {
        url.query_pairs_mut().append_pair("connection_id", id);
    }
    for k in ["cursor", "limit"] {
        if let Some(x) = v.get(k) {
            url.query_pairs_mut().append_pair(
                k,
                &x.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| x.to_string()),
            );
        }
    }
    parts.uri = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
    .parse()
    .unwrap();
    parts
        .headers
        .insert("content-type", "application/json".parse().unwrap());
    parts.headers.remove("content-length");
    let response = next
        .run(Request::from_parts(parts, Body::from(v.to_string())))
        .await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 32 * 1024 * 1024)
        .await
        .unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if v.is_null() && !matches!(op.as_str(), "RevokeGrant" | "RevokeCredentials") {
        return Response::builder()
            .status(status)
            .header("content-type", "application/proto")
            .body(Body::from(bytes))
            .unwrap();
    }
    if !status.is_success() {
        let failure = h::ApiErrorBody {
            error: parse(v["error"].clone()),
            request_id: String::new(),
        };
        let error = connectrpc::ConnectError::new(
            connectrpc::ErrorCode::FailedPrecondition,
            &failure.error.message,
        )
        .with_detail(connectrpc::ErrorDetail::from_message(
            "ene.host.v1.ApiFailure",
            &w::ApiFailure {
                error: failure.error.into_wire().unwrap().into(),
                http_status: u32::from(status.as_u16()),
                ..Default::default()
            },
        ));
        return error
            .into_http_response(&axum::http::HeaderMap::new())
            .map(Body::new);
    }
    Response::builder()
        .header("content-type", "application/proto")
        .body(Body::from(encode_response(&op, v)))
        .unwrap()
}
