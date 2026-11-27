use super::*;
use ene_domain::settings::{ConnectionConfig, CredentialState, Endpoint, SttConnection};

#[test]
fn select_connection_prefers_enabled_and_select_by_id_rejects_disabled() {
    let disabled = Connection {
        id: ene_domain::ConnectionId::parse("stt-disabled").unwrap(),
        name: "Disabled STT".into(),
        enabled: false,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "openai".into(),
                endpoint: "http://127.0.0.1:8080/v1".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec!["ja".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    };
    let enabled = Connection {
        id: ene_domain::ConnectionId::parse("stt-enabled").unwrap(),
        name: "Enabled STT".into(),
        enabled: true,
        kind: ConnectionKind::Stt,
        config: ConnectionConfig::Stt(SttConnection {
            endpoint: Endpoint {
                adapter: "openai".into(),
                endpoint: "http://127.0.0.1:8081/v1".into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            language_hints: vec!["ja".into()],
        }),
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
    };

    let conns = vec![disabled.clone(), enabled];
    assert_eq!(
        select_connection(&conns, ConnectionKind::Stt)
            .unwrap()
            .id
            .as_str(),
        "stt-enabled"
    );
    assert!(matches!(
        select_connection(&[disabled], ConnectionKind::Stt),
        Err(SetupError::Disabled(_))
    ));
    assert!(matches!(
        select_connection(&conns, ConnectionKind::Tts),
        Err(SetupError::Missing)
    ));
    assert!(matches!(
        select_connection_by_id(&conns, ConnectionKind::Stt, "stt-disabled"),
        Err(SetupError::Disabled(_))
    ));
    assert_eq!(
        select_connection_by_id(&conns, ConnectionKind::Stt, "stt-enabled")
            .unwrap()
            .id
            .as_str(),
        "stt-enabled"
    );
}
