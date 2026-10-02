use super::*;
use crate::ports::llm::Policy;
use serde_json::json;
pub fn connection(c: &Configuration, id: &str) -> Option<ProviderConnection> {
    c.settings.connections.iter().find(|p| p.id == id).cloned()
}

pub fn allowed(
    c: &Configuration,
    connection: &ProviderConnection,
    purpose: &str,
    data: &[&str],
    policy: &Policy,
) -> bool {
    c.grants.iter().any(|g| {
        g.status == GrantStatus::Active
            && g.companion_id == c.companion.id
            && g.kind == "data_transfer"
            && g.connection_id == connection.id
            && g.purpose.as_str() == purpose
            && g.connection_fingerprint == policy.fingerprint
            && policy
                .destinations
                .iter()
                .all(|d| g.destinations.iter().any(|v| v == d))
            && data.iter().all(|d| g.data_kinds.iter().any(|v| v == d))
    })
}

pub fn refresh(c: &mut Configuration, policy: Option<&Policy>) {
    let assignment = c.companion.model_assignments.companion.as_ref();
    let reason = match assignment
        .map(|a| a.connection_id.as_str())
        .and_then(|id| connection(c, id))
    {
        None => Some("model_unset"),
        Some(_p) if policy.is_none() => Some("provider_unavailable"),
        Some(p)
            if p.credential_state == "missing"
                && policy.is_some_and(|p| p.requires_credentials) =>
        {
            Some("credential_missing")
        }
        Some(p)
            if !policy.is_some_and(|policy| {
                allowed(c, &p, "conversation", &["conversation"], policy)
            }) =>
        {
            Some("permission_required")
        }
        _ => None,
    };
    c.companion.availability = json!({"chat":{"ready":reason.is_none(),"reasons":reason.map(|r|vec![json!({"code":r,"message":"接続・モデル・資格情報・送信許可を設定してください"})]).unwrap_or_default()},"task":{"ready":false,"reasons":[{"code":"unsupported_capability","message":"ファイル作業は未対応です"}]},"speech":{"ready":false,"reasons":[{"code":"unsupported_capability","message":"読み上げは未対応です"}]}});
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_covers_all_destinations_data_purpose_and_implementation() {
        let mut c:Configuration=serde_json::from_value(json!({"companion":{"id":"c","name":"ene","body":null,"voice":null,"mind":"日本語","model_assignments":{},"availability":{},"revision":1},"settings":{"revision":1,"connections":[]},"grants":[{"id":"g","companion_id":"c","kind":"data_transfer","connection_id":"p","destinations":["https://gateway.test","processor"],"data_kinds":["conversation"],"purpose":"conversation","status":"active","revision":1,"invalidation_reason":null,"connection_fingerprint":{"implementation":"v1"}}]})).unwrap();
        let p = ProviderConnection {
            id: "p".into(),
            kind: "llm".into(),
            adapter: "plugin".into(),
            endpoint: "https://gateway.test".into(),
            options: json!({}),
            credential_state: "missing".into(),
            capabilities: None,
            last_check: None,
        };
        let mut policy = Policy {
            fingerprint: json!({"implementation":"v1"}),
            destinations: vec!["https://gateway.test".into(), "processor".into()],
            requires_credentials: true,
            environment_variable: None,
        };
        assert!(allowed(&c, &p, "conversation", &["conversation"], &policy));
        assert!(!allowed(
            &c,
            &p,
            "connection_check",
            &["test_input"],
            &policy
        ));
        assert!(!allowed(&c, &p, "conversation", &["metadata"], &policy));
        c.grants[0].destinations.pop();
        assert!(!allowed(&c, &p, "conversation", &["conversation"], &policy));
        c.grants[0].destinations.push("processor".into());
        policy.fingerprint = json!({"implementation":"v2"});
        assert!(!allowed(&c, &p, "conversation", &["conversation"], &policy));
    }
}
