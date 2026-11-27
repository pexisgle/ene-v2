use super::*;
use crate::host::providers::*;
use crate::host::{
    mind::Role,
    model::Credentials,
    settings::{
        self, Connection, ConnectionConfig, ConnectionKind, CredentialState, EmbeddingConnection,
        Endpoint,
    },
    voice::{self, SetupError},
};
use bevy_ecs::world::World;
use rig_core::{catalog::Catalog, completion::CompletionRequest};
use serde_json::json;

fn grant_for(
    provider: &ene_plugin::ProviderConfiguration,
    connection: &Connection,
    credentials: Option<&Credentials>,
    providers: &Providers,
) -> Grant {
    let secret = model::connection_secret(connection, credentials, providers).unwrap();
    Grant::new(provider, secret)
}

#[tokio::test]
async fn p10_listing_uses_authorized_http_and_connection_credentials() {
    use base64::Engine;
    let native = NativeEndpoint::start().await;
    let upstream = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let secret = "listing-host-secret";
    let credentials = Credentials::new(true);
    credentials.remember("another-connection", "wrong-secret".into());
    credentials.remember(connection.id.as_str(), secret.into());
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(br#"{"data":[{"id":"native-model"}]}"#);
    let body = [
        assert_guest_bytes(&format!("\"endpoint\":\"{}\"", native.base), true, true),
        assert_guest_bytes(r#""options":{"fixture_option":"preserved"}"#, true, true),
        assert_guest_bytes(secret, true, false),
        assert_guest_bytes("replacement-secret", true, false),
        service(&json!({"type":"http_open", "request":{
            "method":"GET", "url":format!("{}/models", upstream.base), "headers":{}
        }})),
        assert_guest_bytes(r#""Ok":{"#, false, true),
        service(&json!({"type":"http_read", "handle":"http-1"})),
        assert_guest_bytes(&format!("\"data_base64\":\"{encoded}\""), false, true),
        assert_guest_bytes(r#""done":true"#, false, true),
        assert_guest_bytes(secret, false, false),
        assert_guest_bytes("replacement-secret", false, false),
        assert_guest_bytes("private-response", false, false),
        assert_guest_bytes("set-cookie", false, false),
        service(&json!({"type":"http_close", "handle":"http-1"})),
        assert_guest_bytes(r#"{"Ok":null}"#, false, true),
        emit(r#"{"type":"models","models":[{"id":"plugin-http-model","name":"HTTP"}]}"#),
        reply("{}"),
    ]
    .join(" ");
    let providers = installed_providers(
        root.path(),
        Extension::Llm,
        &body,
        &upstream.base,
        &json!({"type":"bearer"}),
    );
    for key in [secret, "replacement-secret"] {
        credentials.remember(connection.id.as_str(), key.into());
        let check = model::probe(&credentials, &connection, &providers).await;
        assert!(check.healthy, "{:?}", check.failure);
        assert!(check.resources.contains(&"plugin-http-model".into()));
        assert!(!check.resources.contains(&"native-model".into()));
        assert_eq!(
            upstream.headers.lock().last().unwrap()["authorization"]
                .to_str()
                .unwrap(),
            format!("Bearer {key}")
        );
    }
    upstream.assert_requests(2);
    native.assert_requests(0);
}

#[tokio::test]
async fn p10_listing_http_failures_never_select_native_or_expand_origins() {
    for denied in [true, false] {
        let native = NativeEndpoint::start().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let approved = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let destination = if denied { &native.base } else { &approved };
        let body = format!(
            "{} local.get $p call $error i32.const 1 return",
            service(&json!({"type":"http_open", "request":{
                "method":"GET", "url":destination, "headers":{}
            }}))
        );
        let providers = providers(root.path(), Extension::Llm, &body, &approved).await;
        let check = model::probe(&Credentials::new(true), &connection, &providers).await;
        assert!(!check.healthy);
        let failure = check.failure.unwrap();
        assert_eq!(failure.code, "connection_check");
        let code = if denied {
            "destination_denied"
        } else {
            "http_failed"
        };
        assert!(failure.message.contains(code), "{}", failure.message);
        native.assert_requests(0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p08_speech_construction_failure_never_selects_native() {
    for (kind, extension) in [
        (ConnectionKind::Tts, Extension::Tts),
        (ConnectionKind::Stt, Extension::Stt),
    ] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(kind, &native.base);
        let store = store(root.path(), &connection);
        let providers = providers(root.path(), extension, &reply("{}"), &native.base).await;
        let mut world = World::new();
        world.insert_resource(store);
        world.insert_resource(providers);
        let _failure = construction_failure::Guard::enter();
        let error = if kind == ConnectionKind::Tts {
            voice::tts_source(&voice::VoiceEngineContext::for_test(&world))
                .err()
                .expect("TTS must fail")
        } else {
            voice::stt_source(&voice::VoiceEngineContext::for_test(&world))
                .err()
                .expect("STT must fail")
        };
        assert!(
            matches!(error, SetupError::Broken(ref message) if message.starts_with("plugin HTTP transport: ")),
            "{error:?}"
        );
        native.assert_requests(0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p08_absent_plugin_keeps_native_completion_and_listing() {
    let native = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &native.base);
    let store = store(root.path(), &connection);
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, connection.id.as_str(), "test-only").unwrap();
    let _failure = construction_failure::Guard::enter();
    let providers = Providers::none();
    let model = model::connect(
        &store,
        &credentials,
        Catalog::builtin(),
        &companion(),
        Role::Companion,
        &providers,
    )
    .unwrap();
    assert_eq!(
        model
            .call(CompletionRequest::new("hello"))
            .await
            .unwrap()
            .text(),
        "native completion"
    );
    let check = model::probe(&credentials, &connection, &providers).await;
    assert!(check.healthy, "{:?}", check.failure);
    assert!(check.resources.contains(&"native-model".into()));
    native.assert_requests(2);
}

#[tokio::test(flavor = "current_thread")]
async fn p08_absent_plugin_leaves_speech_unavailable() {
    for (kind, label) in [
        (ConnectionKind::Tts, "TTS adapter"),
        (ConnectionKind::Stt, "STT adapter"),
    ] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(kind, &native.base);
        let mut world = World::new();
        world.insert_resource(store(root.path(), &connection));
        world.insert_resource(Providers::none());
        let error = if kind == ConnectionKind::Tts {
            voice::tts_source(&voice::VoiceEngineContext::for_test(&world))
                .err()
                .expect("TTS must fail")
        } else {
            voice::stt_source(&voice::VoiceEngineContext::for_test(&world))
                .err()
                .expect("STT must fail")
        };
        let SetupError::Unsupported(message) = &error else {
            panic!("expected an unsupported adapter, got {error:?}")
        };
        assert!(
            message.contains(label) && message.contains("openrouter"),
            "{message}"
        );
        native.assert_requests(0);
    }
}

#[tokio::test]
async fn p08_plugin_serves_tts() {
    let declared = NativeEndpoint::start().await;
    let audio = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Tts, &declared.base);
    let open = json!({"type":"http_open","request":{
        "method":"GET", "url":format!("{}/audio/speech", audio.base), "headers":{}
    }})
    .to_string();
    let body = format!(
        "{} local.get $p call $service drop global.get $calls i32.const 1 i32.eq if {} else {} end",
        guest_write(&open),
        reply(r#"{"handle":"http-1","sample_rate":24000,"format":"pcm16","bytes":2}"#),
        reply(r#"{"handle":"http-2","sample_rate":24000,"format":"pcm16","bytes":2}"#)
    );
    let providers = providers(root.path(), Extension::Tts, &body, &audio.base).await;
    let mut world = World::new();
    world.insert_resource(store(root.path(), &connection));
    world.insert_resource(providers);
    let engine = voice::tts_source(&voice::VoiceEngineContext::for_test(&world)).unwrap();
    for _ in 0..2 {
        let (pcm, rate) = engine.synthesize("hello".into(), "v".into()).await.unwrap();
        assert_eq!(pcm, [7, 0]);
        assert_eq!(rate, 24_000);
    }
    declared.assert_requests(0);
    audio.assert_requests(2);
}

#[test]
fn p08_actual_grant_binds_origins_and_secret_at_construction() {
    use ene_plugin_runtime::service::HttpGrants;
    let connection = connection(ConnectionKind::Llm, "https://connection.example");
    let credentials = Credentials::new(true);
    let providers = Providers::none();
    model::save_secret(&credentials, connection.id.as_str(), "host-secret").unwrap();
    let provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":"https://api.example",
        "auth":{"type":"header", "name":"x-api-key"},
        "destinations":["https://api.example/path", "https://api.example:8443", "not a URL"]
    }))
    .unwrap();
    let grant = grant_for(&provider, &connection, Some(&credentials), &providers);
    for provider_argument in ["owner", "another-provider"] {
        for allowed in [
            "https://api.example/other",
            "https://api.example:443",
            "https://api.example:8443/v1",
        ] {
            assert!(grant.allows(provider_argument, &url::Url::parse(allowed).unwrap()));
        }
        for denied in [
            "http://api.example",
            "https://api.example.evil",
            "https://api.example:8444",
            "https://connection.example",
        ] {
            assert!(!grant.allows(provider_argument, &url::Url::parse(denied).unwrap()));
        }
        let mut headers = reqwest::header::HeaderMap::new();
        grant.authorize(provider_argument, &mut headers).unwrap();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers["x-api-key"], "host-secret");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p08_construction_failure_is_scoped_and_not_an_invalid_destination() {
    use ene_plugin_runtime::service::DenyHttp;
    {
        let _failure = construction_failure::Guard::enter();
        let error = http_broker(Arc::new(DenyHttp)).unwrap_err();
        assert!(error.is_builder());
    }
    let broker = http_broker(Arc::new(DenyHttp)).unwrap();
    let invalid =
        serde_json::from_value(json!({"method":"GET","url":"not a URL","headers":{}})).unwrap();
    assert_eq!(
        broker.open("p", invalid).await.unwrap_err().code,
        "invalid_url"
    );
}

#[tokio::test]
async fn p08_plugin_serves_stt() {
    let declared = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Stt, &declared.base);
    let providers = providers(
        root.path(),
        Extension::Stt,
        &reply(r#"{"text":"plugin transcript"}"#),
        &declared.base,
    )
    .await;
    let mut world = World::new();
    world.insert_resource(store(root.path(), &connection));
    world.insert_resource(providers);
    let engine = voice::stt_source(&voice::VoiceEngineContext::for_test(&world)).unwrap();
    for _ in 0..2 {
        assert_eq!(
            engine.transcribe(vec![0, 0], 24_000).await.unwrap(),
            "plugin transcript"
        );
    }
    declared.assert_requests(0);
}

#[tokio::test]
async fn p08_plugin_generation_failure_never_selects_native() {
    for terminal in [false, true] {
        let native = NativeEndpoint::start().await;
        let root = tempfile::tempdir().unwrap();
        let connection = connection(ConnectionKind::Llm, &native.base);
        let store = store(root.path(), &connection);
        let body = if terminal {
            format!(
                "{} unreachable",
                emit(r#"{"type":"finished","reason":"stop"}"#)
            )
        } else {
            reply("{}")
        };
        let providers = providers(root.path(), Extension::Llm, &body, &native.base).await;
        let model = model::connect(
            &store,
            &Credentials::new(true),
            Catalog::builtin(),
            &companion(),
            Role::Companion,
            &providers,
        )
        .unwrap();
        let error = model
            .call(CompletionRequest::new("hello"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("plugin call"), "{error}");
        native.assert_requests(0);
    }
}

fn request(url: &str) -> ene_plugin::HttpRequest {
    serde_json::from_value(json!({"method":"GET", "url":url, "headers":{}})).unwrap()
}

#[tokio::test]
async fn p08_actual_grant_enforces_transport_policy_and_snapshots_credentials() {
    let upstream = NativeEndpoint::start().await;
    let denied = NativeEndpoint::start().await;
    let providers = Providers::none();
    let connection = connection(ConnectionKind::Llm, &denied.base);
    let credentials = Credentials::new(true);
    model::save_secret(&credentials, connection.id.as_str(), "original-secret").unwrap();
    let provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":upstream.base,
        "auth":{"type":"bearer"}, "destinations":[upstream.base]
    }))
    .unwrap();
    let broker = http_broker(Arc::new(grant_for(
        &provider,
        &connection,
        Some(&credentials),
        &providers,
    )))
    .unwrap();
    assert_eq!(
        broker
            .open("owner", request(&denied.base))
            .await
            .unwrap_err()
            .code,
        "destination_denied"
    );
    for header in [
        "AuThOrIzAtIoN",
        "pRoXy-AuThOrIzAtIoN",
        "CoOkIe",
        "HoSt",
        "CoNtEnT-LeNgTh",
    ] {
        let mut forbidden = request(&upstream.base);
        forbidden
            .headers
            .insert(header.into(), "guest-value".into());
        assert_eq!(
            broker.open("owner", forbidden).await.unwrap_err().code,
            "reserved_header"
        );
    }
    upstream.assert_requests(0);
    denied.assert_requests(0);
    model::save_secret(&credentials, connection.id.as_str(), "replacement-secret").unwrap();
    for provider_argument in ["owner", "foreign-provider"] {
        let opened = broker
            .open(provider_argument, request(&upstream.base))
            .await
            .unwrap();
        let chunk = broker.read(&opened.handle).await.unwrap();
        assert_eq!(chunk.headers["x-safe"], "visible");
        assert!(!chunk.headers.contains_key("authorization"));
        assert!(!chunk.headers.contains_key("set-cookie"));
        assert_eq!(
            upstream.headers.lock().last().unwrap()["authorization"],
            "Bearer original-secret"
        );
    }
    let fresh = http_broker(Arc::new(grant_for(
        &provider,
        &connection,
        Some(&credentials),
        &providers,
    )))
    .unwrap();
    fresh.open("owner", request(&upstream.base)).await.unwrap();
    assert_eq!(
        upstream.headers.lock().last().unwrap()["authorization"],
        "Bearer replacement-secret"
    );
    model::delete_secret(&credentials, connection.id.as_str()).unwrap();
    broker.open("owner", request(&upstream.base)).await.unwrap();
    assert_eq!(
        upstream.headers.lock().last().unwrap()["authorization"],
        "Bearer original-secret"
    );
    upstream.assert_requests(4);
    denied.assert_requests(0);
}

#[tokio::test]
async fn p08_actual_grant_overwrites_guest_auth_and_refuses_invalid_auth() {
    let upstream = NativeEndpoint::start().await;
    let connection = connection(ConnectionKind::Llm, &upstream.base);
    let credentials = Credentials::new(true);
    let providers = Providers::none();
    model::save_secret(&credentials, connection.id.as_str(), "host-secret").unwrap();
    let mut provider: ProviderConfiguration = serde_json::from_value(json!({
        "id":"owner", "endpoint":upstream.base,
        "auth":{"type":"header", "name":"x-api-key"}, "destinations":[upstream.base]
    }))
    .unwrap();
    let broker = http_broker(Arc::new(grant_for(
        &provider,
        &connection,
        Some(&credentials),
        &providers,
    )))
    .unwrap();
    let mut call = request(&upstream.base);
    call.headers
        .insert("X-Api-Key".into(), "guest-secret".into());
    broker.open("foreign-provider", call).await.unwrap();
    assert_eq!(
        upstream.headers.lock().last().unwrap()["x-api-key"],
        "host-secret"
    );
    for (auth, expected) in [
        (json!({"type":"query", "name":"key"}), "unsupported_auth"),
        (
            json!({"type":"header", "name":"bad\nname"}),
            "invalid_auth_header",
        ),
    ] {
        provider.auth = serde_json::from_value(auth).unwrap();
        let broker = http_broker(Arc::new(grant_for(
            &provider,
            &connection,
            Some(&credentials),
            &providers,
        )))
        .unwrap();
        assert_eq!(
            broker
                .open("owner", request(&upstream.base))
                .await
                .unwrap_err()
                .code,
            expected
        );
    }
    provider.auth = serde_json::from_value(json!({"type":"bearer"})).unwrap();
    model::save_secret(&credentials, connection.id.as_str(), "bad\nsecret").unwrap();
    let broker = http_broker(Arc::new(grant_for(
        &provider,
        &connection,
        Some(&credentials),
        &providers,
    )))
    .unwrap();
    assert_eq!(
        broker
            .open("owner", request(&upstream.base))
            .await
            .unwrap_err()
            .code,
        "invalid_auth"
    );
    upstream.assert_requests(1);
}

#[tokio::test]
async fn p08_provider_replies_are_owned_by_one_live_invocation() {
    use ene_plugin::{Operation, ServiceRequest};
    use ene_plugin_runtime::instance::Services;
    let upstream = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let connection = connection(ConnectionKind::Llm, &upstream.base);
    let open = json!({"type":"http_open", "request":{
        "method":"GET", "url":upstream.base, "headers":{}
    }})
    .to_string();
    let body = format!(
        "{} local.get $p call $service drop {} {}",
        guest_write(&open),
        emit(r#"{"type":"models","models":[]}"#),
        reply("{}")
    );
    let providers = providers(root.path(), Extension::Llm, &body, &upstream.base).await;
    let running = providers.running("openrouter", Extension::Llm).unwrap();
    let context = crate::host::providers::ProviderInvocationContext::from_connection(
        &running,
        &connection,
        None,
        &providers,
        None,
    )
    .unwrap();
    let first = context.services();
    let second = context.services();
    for services in [&first, &second] {
        running
            .invoke(
                Operation::ListModels {
                    endpoint: upstream.base.clone(),
                    options: json!({}),
                },
                services.clone(),
            )
            .await
            .unwrap();
    }
    for (services, handle) in [(&first, "http-2"), (&second, "http-1")] {
        assert_eq!(
            services.broker.read(handle).await.unwrap_err().code,
            "unknown_handle"
        );
    }
    let dropped = Arc::downgrade(&first);
    drop(first);
    assert!(dropped.upgrade().is_none());
    providers.set_enabled("openrouter", false);
    assert!(providers.running("openrouter", Extension::Llm).is_none());
    let later = context.services();
    assert_eq!(later.check().await.unwrap_err().code, "provider_revoked");
    assert_eq!(
        second
            .handle(ServiceRequest::HttpRead {
                handle: "http-2".into()
            })
            .await
            .unwrap_err()
            .code,
        "provider_revoked"
    );
    drop(context);
    drop(running);
    drop(providers);
    assert_eq!(
        second.broker.read("http-2").await.unwrap_err().code,
        "provider_revoked"
    );
    upstream.assert_requests(2);
}

/// A plugin the operator installed has to be visible in the catalogue and has
/// to answer for its own defaults, or a connection that names it can never be
/// filled in from the settings screen.
#[test]
fn p10_listed_providers_cover_installed_plugins_and_their_defaults() {
    let root = tempfile::tempdir().unwrap();
    let providers = installed_providers(
        root.path(),
        Extension::Llm,
        &reply("{}"),
        "https://plugin.example",
        &json!({"type":"none"}),
    );
    let listed = settings::providers(&providers);
    let builtins = [
        "anthropic",
        "gemini",
        "google-agent-platform",
        "openai",
        "sakura",
    ];
    assert_eq!(listed.len(), builtins.len() + 1);
    let mut shadowing: Vec<_> = listed
        .iter()
        .filter(|provider| provider.adapter == "openrouter")
        .collect();
    assert_eq!(
        shadowing.len(),
        1,
        "the plugin must replace the built-in it shadows"
    );
    let plugin = shadowing.remove(0);
    assert_eq!(plugin.id, "p08-plugin");
    assert_eq!(plugin.endpoint, "https://plugin.example");
    assert_eq!(plugin.environment_variable.as_deref(), Some("ENE_P08_KEY"));
    for adapter in builtins {
        assert!(
            listed.iter().any(|provider| provider.adapter == adapter),
            "{adapter}"
        );
    }
    let empty = connection(ConnectionKind::Llm, "");
    assert_eq!(
        settings::endpoint_of(&empty, &providers).as_deref(),
        Some("https://plugin.example")
    );
    assert_eq!(
        settings::environment_variable(&empty, &providers).as_deref(),
        Some("ENE_P08_KEY")
    );
    let configured = connection(ConnectionKind::Llm, "https://operator.example/");
    assert_eq!(
        settings::endpoint_of(&configured, &providers).as_deref(),
        Some("https://operator.example")
    );
}

#[tokio::test]
async fn b07_embedding_plugin_uses_provider_context_and_broker() {
    let manifest_unused = NativeEndpoint::start().await;
    let upstream_a = NativeEndpoint::start().await;
    let upstream_b = NativeEndpoint::start().await;
    let root = tempfile::tempdir().unwrap();
    let call_one = [
        assert_guest_bytes(&format!("\"endpoint\":\"{}\"", upstream_a.base), true, true),
        assert_guest_bytes(r#""fixture_option":"preserved""#, true, true),
        assert_guest_bytes(r#""tier":"a""#, true, true),
        assert_guest_bytes("secret-a", true, false),
        service(&json!({"type":"http_open", "request":{
            "method":"POST", "url":format!("{}/embeddings", upstream_a.base), "headers":{}
        }})),
        assert_guest_bytes(r#""Ok":{"#, false, true),
        service(&json!({"type":"http_close", "handle":"http-1"})),
        assert_guest_bytes(r#"{"Ok":null}"#, false, true),
        reply(r#"{"vectors":[[0.1,0.2,0.3]]}"#),
    ]
    .join(" ");
    let call_two = [
        assert_guest_bytes(&format!("\"endpoint\":\"{}\"", upstream_b.base), true, true),
        assert_guest_bytes(r#""fixture_option":"overridden""#, true, true),
        assert_guest_bytes(r#""tier":"b""#, true, true),
        assert_guest_bytes("secret-b", true, false),
        service(&json!({"type":"http_open", "request":{
            "method":"POST", "url":format!("{}/embeddings", upstream_b.base), "headers":{}
        }})),
        assert_guest_bytes(r#""Ok":{"#, false, true),
        service(&json!({"type":"http_close", "handle":"http-1"})),
        assert_guest_bytes(r#"{"Ok":null}"#, false, true),
        reply(r#"{"vectors":[[0.4,0.5,0.6]]}"#),
    ]
    .join(" ");
    let body = format!("global.get $calls i32.const 1 i32.eq if {call_one} else {call_two} end");
    let _discarded = installed_providers(
        root.path(),
        Extension::Embedding,
        &body,
        &manifest_unused.base,
        &json!({"type":"bearer"}),
    );
    let manifest_path = root
        .path()
        .join(PLUGINS_DIR)
        .join("openrouter/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["entrypoints"][0]["provider"]["destinations"] =
        json!([upstream_a.base, upstream_b.base]);
    super::tests::write(manifest_path, manifest.to_string());
    let providers = Providers::load(root.path());

    let conn_a = Connection {
        id: ene_domain::ConnectionId::parse("emb-a").unwrap(),
        name: "Embed A".into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "openrouter".into(),
                endpoint: upstream_a.base.clone(),
                options: r#"{"tier":"a"}"#.into(),
                ..Default::default()
            },
            model_id: "embed-a".into(),
            dimensions: 3,
        }),
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let conn_b = Connection {
        id: ene_domain::ConnectionId::parse("emb-b").unwrap(),
        name: "Embed B".into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "openrouter".into(),
                endpoint: upstream_b.base.clone(),
                options: r#"{"tier":"b","fixture_option":"overridden"}"#.into(),
                ..Default::default()
            },
            model_id: "embed-b".into(),
            dimensions: 3,
        }),
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
    };
    let credentials = Credentials::new(true);
    credentials.remember(conn_a.id.as_str(), "secret-a".into());
    credentials.remember(conn_b.id.as_str(), "secret-b".into());

    let dims_a = crate::host::mind::embedding::probe_dimensions(&conn_a, &credentials, &providers)
        .await
        .unwrap();
    let dims_b = crate::host::mind::embedding::probe_dimensions(&conn_b, &credentials, &providers)
        .await
        .unwrap();
    assert_eq!(dims_a, 3);
    assert_eq!(dims_b, 3);
    manifest_unused.assert_requests(0);
    upstream_a.assert_requests(1);
    upstream_b.assert_requests(1);
    assert_eq!(
        upstream_a.headers.lock()[0]["authorization"],
        "Bearer secret-a"
    );
    assert_eq!(
        upstream_b.headers.lock()[0]["authorization"],
        "Bearer secret-b"
    );
}
