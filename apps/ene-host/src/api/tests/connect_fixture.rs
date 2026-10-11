use buffa::Message;
use ene_rpc as w;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
fn encode<T: Message + DeserializeOwned>(v: Value) -> Vec<u8> {
    serde_json::from_value::<T>(v).unwrap().encode_to_vec()
}
fn decode<T: Message + Serialize>(b: &[u8]) -> Value {
    normalized(serde_json::to_value(T::decode_from_slice(b).unwrap()).unwrap())
}
pub fn normalized(value: Value) -> Value {
    match value {
        Value::Object(fields) if fields.len() == 1 && fields.contains_key("json") => {
            serde_json::from_str(fields["json"].as_str().unwrap()).unwrap()
        }
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(key, mut value)| {
                    let key = key
                        .chars()
                        .flat_map(|c| {
                            if c.is_ascii_uppercase() {
                                vec!['_', c.to_ascii_lowercase()]
                            } else {
                                vec![c]
                            }
                        })
                        .collect::<String>();
                    if matches!(
                        key.as_str(),
                        "revision" | "seq" | "sequence" | "content_chunks" | "unicode_scalars"
                    ) && let Some(number) = value.as_str().and_then(|v| v.parse::<u64>().ok())
                    {
                        value = json!(number);
                    }
                    (key, normalized(value))
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(normalized).collect()),
        value => value,
    }
}
pub fn encode_request(op: &str, mut v: Value) -> Vec<u8> {
    if v.is_null() {
        v = json!({});
    }
    match op {
        "UpdateSettings" => {
            for c in v["connections"].as_array_mut().unwrap() {
                c["options"] = json!({"json":c["options"].to_string()});
            }
            encode::<w::UpdateSettingsRequest>(v)
        }
        "UpdateCompanion" => {
            if let Some(Value::Object(assignments)) =
                v.as_object_mut().unwrap().remove("model_assignments")
            {
                for slot in ["companion", "task"] {
                    if let Some(assignment) = assignments.get(slot) {
                        v[format!("{slot}_assignment")] = if assignment.is_null() {
                            json!({"clear":{}})
                        } else {
                            json!({"set":assignment})
                        };
                    }
                }
            }
            encode::<w::UpdateCompanionRequest>(v)
        }
        "SendMessage" => encode::<w::SendMessageRequest>(v),
        "CreateGrant" => encode::<w::CreateGrantRequest>(v),
        "UpdateGrant" => {
            for key in ["destinations", "data_kinds"] {
                if let Some(items) = v.get(key).cloned() {
                    v[key] = json!({"values":items});
                }
            }
            encode::<w::UpdateGrantRequest>(v)
        }
        "RegisterCredentials" => encode::<w::CredentialRequest>(v),
        "CheckConnection" => encode::<w::CheckRequest>(v),
        "RevokeGrant" | "RevokeCredentials" => encode::<w::Id>(v),
        _ => panic!("unmapped request {op}"),
    }
}
pub fn decode_response(op: &str, bytes: &[u8]) -> Value {
    match op {
        "UpdateSettings" => {
            let mut value = decode::<w::Settings>(bytes);
            if value.get("connections").is_none() {
                value["connections"] = json!([]);
            }
            value
        }
        "UpdateCompanion" => decode::<w::Companion>(bytes),
        "SendMessage" => decode::<w::MessageAcceptance>(bytes),
        "RegisterCredentials" => decode::<w::CredentialResult>(bytes),
        "CreateGrant" | "UpdateGrant" => decode::<w::GrantResult>(bytes),
        "CheckConnection" => decode::<w::CheckResult>(bytes),
        "RevokeGrant" | "RevokeCredentials" => decode::<w::Empty>(bytes),
        _ => panic!("unmapped response {op}"),
    }
}
