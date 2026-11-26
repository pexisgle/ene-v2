use super::*;
use crate::host::settings::{CredentialState, Endpoint, LlmConnection};
use rig_core::providers::openai::wire::DimensionsField;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn llm_connection(adapter: &str, endpoint: &str, models: &[&str]) -> Connection {
    Connection {
        id: ene_domain::ConnectionId::parse("c1").unwrap(),
        name: "main".into(),
        enabled: true,
        kind: ConnectionKind::Llm,
        config: ConnectionConfig::Llm(LlmConnection {
            endpoint: Endpoint {
                adapter: adapter.into(),
                endpoint: endpoint.into(),
                options_schema: String::new(),
                options: "{}".into(),
            },
            model_ids: models.iter().map(|model| (*model).to_owned()).collect(),
        }),
        credential_state: CredentialState::Absent,
        revision: 1,
        last_check: None,
    }
}

fn connection_for(adapter: &str, endpoint: &str) -> Connection {
    llm_connection(adapter, endpoint, &["gpt-5-mini"])
}

/// A store that cannot be read must not look like a connection with no
/// credential: the request path has to report the failure instead of
/// sending an unauthenticated request or falling back to the environment.
#[test]
fn an_unreadable_credential_store_is_not_an_absent_credential() {
    let credentials = Credentials::new(false);
    credentials.make_unreadable();
    let connection = connection_for("openai", "http://127.0.0.1:1234");
    let providers = super::super::providers::Providers::none();
    assert!(matches!(
        credentials.secret(connection.id.as_str()),
        Err(HostError::Unavailable(_))
    ));
    assert!(connection_secret(&connection, Some(&credentials), &providers).is_err());
    assert!(credential_state(&connection, &credentials, &providers).is_err());
}

#[test]
fn registry_configs_keep_native_defaults_and_custom_endpoints() {
    let providers = super::super::providers::Providers::none();
    for adapter in ["openai", "openrouter", "anthropic", "gemini"] {
        let base = super::super::settings::provider(adapter, &providers)
            .unwrap()
            .endpoint;
        let preset = provider_of(adapter).unwrap().config("key").unwrap();
        let config = provider_config(adapter, &base, Some("key".into()), &providers);
        assert_eq!(config, preset, "{adapter}");
        let custom = provider_config(
            adapter,
            "http://127.0.0.1:1234",
            Some("key".into()),
            &providers,
        );
        assert_eq!(
            custom,
            preset.with_base_url("http://127.0.0.1:1234"),
            "{adapter}"
        );
    }
    assert_eq!(
        provider_config("custom", "http://127.0.0.1:1234", None, &providers),
        ProviderConfig::OpenAi(OpenAIConfig::new("").with_base_url("http://127.0.0.1:1234"))
    );
}

#[tokio::test]
async fn completion_keeps_catalog_overrides_and_unknown_tool_support() {
    use crate::host::{mind::ModelAssignment, tests::Harness};
    use rig_core::catalog::Pricing;

    let harness = Harness::new([]);
    let store = harness.store();
    let mut companion = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    let credentials = Credentials::new(true);
    let providers = super::super::providers::Providers::none();
    let mut overrides = Catalog::default();
    let original = Catalog::builtin()
        .resolve("openai/gpt-4o-mini")
        .unwrap()
        .spec
        .clone();
    let override_pricing = Pricing::new(1.25, 5.0);
    let spec = original.clone().with_pricing(override_pricing);
    overrides.insert(spec.clone());
    let catalog = Catalog::builtin().with_overrides(&overrides);
    for adapter in ["openai", "gemini"] {
        let mut connection = connection_for(adapter, "http://127.0.0.1:1234");
        connection.id = ene_domain::ConnectionId::parse(adapter).unwrap();
        credentials.remember(adapter, "explicit-key".into());
        store
            .settings_repo()
            .update_settings(
                store.settings_repo().settings_revision().unwrap(),
                &[(connection, 0)],
            )
            .unwrap();
        for id in ["gpt-4o-mini", "ene-unlisted-model"] {
            companion.assignments.companion = ModelAssignment {
                connection_id: adapter.into(),
                model_id: id.into(),
            };
            let model = connect(
                &store,
                &credentials,
                &catalog,
                &companion,
                Role::Companion,
                &providers,
            )
            .unwrap();
            let connected = model.spec().unwrap();
            assert_eq!(connected.id, id);
            assert_eq!(connected.provider, provider_of(adapter).unwrap());
            if adapter == "openai" && id == "gpt-4o-mini" {
                let pricing = connected.pricing.as_ref().unwrap();
                assert_eq!(
                    (pricing.input, pricing.output),
                    (override_pricing.input, override_pricing.output)
                );
                assert_eq!(
                    pricing.cache_read,
                    original.pricing.as_ref().unwrap().cache_read
                );
                assert_eq!(connected.context_window, original.context_window);
            } else {
                assert!(connected.tools);
            }
        }
    }
}

#[test]
fn sakura_perception_exposes_vision_only_for_supported_models() {
    use crate::host::{mind::ModelAssignment, tests::Harness};

    let harness = Harness::new([]);
    let store = harness.store();
    let mut companion = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    let connection = connection_for("sakura", "http://127.0.0.1:1234");
    let connection_id = connection.id.clone();
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(connection, 0)],
        )
        .unwrap();
    for (id, supports_image) in [
        ("preview/DeepSeek-V4.1-Flash-1017", true),
        ("gpt-oss-120b", false),
    ] {
        companion.assignments.perception = Some(ModelAssignment {
            connection_id: connection_id.as_str().to_owned(),
            model_id: id.into(),
        });
        let model = connect(
            &store,
            &Credentials::new(true),
            Catalog::builtin(),
            &companion,
            Role::Perception,
            &super::super::providers::Providers::none(),
        )
        .unwrap();
        let spec = model
            .spec()
            .expect("sakura completion model must expose catalog facts");
        assert_eq!(spec.id, id);
        assert_eq!(spec.provider.vendor(), "sakura");
        assert!(spec.tools);
        assert_eq!(spec.input.image, supports_image);
    }
}

/// Every built-in LLM provider binds through `agent::ensure_model` for
/// both companion and task roles, including catalog-unlisted models and
/// provider-specific models returned by model listing.
#[test]
fn all_builtin_providers_bind_through_ensure_model_for_companion_and_task() {
    use crate::host::{agent, mind::ModelAssignment, notify::Notifier};
    use rig_ecs::model::{Connection as ModelConnection, Models};

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let credentials = Credentials::new(true);
    let cases: &[(&str, &str, &str)] = &[
        ("openai", "https://api.openai.com/v1", "ene-unlisted-openai"),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "vendor/ene-unlisted-openrouter",
        ),
        (
            "anthropic",
            "https://api.anthropic.com",
            "claude-ene-unlisted",
        ),
        (
            "gemini",
            "https://generativelanguage.googleapis.com",
            "gemini-ene-unlisted",
        ),
        (
            agent_platform::ADAPTER,
            agent_platform::ENDPOINT,
            "gemini-4-neon",
        ),
        (
            agent_platform::ADAPTER,
            agent_platform::ENDPOINT,
            agent_platform::DEFAULT_MODEL,
        ),
        (
            "sakura",
            super::super::settings::SAKURA_AI_ENDPOINT,
            "gpt-oss-120b",
        ),
        (
            "sakura",
            super::super::settings::SAKURA_AI_ENDPOINT,
            "preview/DeepSeek-V4.1-Flash-1017",
        ),
    ];

    for (index, (adapter, endpoint, model_id)) in cases.iter().enumerate() {
        let conn_id = format!("conn-{index}");
        let mut connection = llm_connection(adapter, endpoint, &[*model_id]);
        connection.id = ene_domain::ConnectionId::parse(&conn_id).unwrap();
        credentials.remember(&conn_id, "test-secret".into());
        store
            .settings_repo()
            .update_settings(
                store.settings_repo().settings_revision().unwrap(),
                &[(connection, 0)],
            )
            .unwrap();

        let assignment = ModelAssignment {
            connection_id: conn_id,
            model_id: (*model_id).to_owned(),
        };
        let mut world = bevy_ecs::prelude::World::new();
        world.insert_resource(store.clone());
        world.insert_resource(credentials.clone());
        world.insert_resource(super::super::providers::Providers::none());
        world.insert_resource(ModelFactory::default());
        world.insert_resource(Models::default());
        world.insert_resource(Notifier::new());

        let created = crate::host::mind::create(
            &mut world,
            ene_rpc::CreateCompanionRequest {
                name: format!("Ene-{index}"),
                mind: "helpful".into(),
                assignments: Some(ene_rpc::ModelAssignments::from(
                    &crate::host::mind::ModelAssignments {
                        companion: assignment.clone(),
                        task: assignment,
                        ..Default::default()
                    },
                ))
                .into(),
                ..Default::default()
            },
        )
        .unwrap();
        let companion = store
            .companion_repo()
            .companion(&created.id)
            .unwrap()
            .unwrap();
        let companion_entity = agent::agent_of(&mut world, &companion.id).expect("companion agent");
        agent::ensure_model(&mut world, companion_entity).unwrap_or_else(|error| {
            panic!("companion ensure_model failed for {adapter}/{model_id}: {error}")
        });
        let companion_spec = &world.get::<ModelConnection>(companion_entity).unwrap().spec;
        assert_eq!(companion_spec.id, *model_id, "{adapter}/{model_id}");
        assert!(companion_spec.tools, "{adapter}/{model_id}");

        let work_entity = agent::spawn_work_agent(
            &mut world,
            &companion,
            &format!("work-{index}"),
            &format!("run-{index}"),
            "do work",
        );
        agent::ensure_model(&mut world, work_entity).unwrap_or_else(|error| {
            panic!("task ensure_model failed for {adapter}/{model_id}: {error}")
        });
        let work_spec = &world.get::<ModelConnection>(work_entity).unwrap().spec;
        assert_eq!(work_spec.id, *model_id, "{adapter}/{model_id}");
        assert!(work_spec.tools, "{adapter}/{model_id}");
    }
}

#[tokio::test]
async fn sakura_companion_binds_and_answers_over_chat_completions() {
    use crate::host::{agent, mind::ModelAssignment, notify::Notifier};
    use rig_core::{
        completion::CompletionRequest,
        effect::{Answered, EffectKind},
        serve::serve_inline,
    };
    use rig_ecs::model::{Connection as ModelConnection, Models};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let read = stream.read(&mut buf).await.unwrap();
        let request = String::from_utf8_lossy(&buf[..read]);
        assert!(
            request.starts_with("POST /v1/chat/completions "),
            "unexpected request: {request}"
        );
        let body = serde_json::json!({
            "id": "chatcmpl-sakura-1",
            "object": "chat.completion",
            "created": 1_700_000_000,
            "model": "gpt-oss-120b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "こんにちは！" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
        })
        .to_string();
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
    });

    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let credentials = Credentials::new(true);
    let mut connection = llm_connection("sakura", &format!("http://{addr}/v1"), &[]);
    connection.id = ene_domain::ConnectionId::parse("sakura-conn").unwrap();
    credentials.remember("sakura-conn", "sakura-key".into());
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(connection, 0)],
        )
        .unwrap();

    let assignment = ModelAssignment {
        connection_id: "sakura-conn".into(),
        model_id: "gpt-oss-120b".into(),
    };
    let mut world = bevy_ecs::prelude::World::new();
    world.insert_resource(store);
    world.insert_resource(credentials);
    world.insert_resource(super::super::providers::Providers::none());
    world.insert_resource(ModelFactory::default());
    world.insert_resource(Models::default());
    world.insert_resource(Notifier::new());

    let created = crate::host::mind::create(
        &mut world,
        ene_rpc::CreateCompanionRequest {
            name: "Ene".into(),
            mind: "helpful".into(),
            assignments: Some(ene_rpc::ModelAssignments::from(
                &crate::host::mind::ModelAssignments {
                    companion: assignment.clone(),
                    task: assignment,
                    ..Default::default()
                },
            ))
            .into(),
            ..Default::default()
        },
    )
    .unwrap();
    let entity = agent::agent_of(&mut world, &created.id).expect("companion agent");
    let bound = world
        .get::<ModelConnection>(entity)
        .expect("sakura companion model must bind without catalog-facts error");
    assert_eq!(bound.spec.id, "gpt-oss-120b");
    assert_eq!(bound.spec.provider.vendor(), "sakura");
    assert!(bound.spec.tools);

    let handler = bound.handler.0.clone();
    let answer = serve_inline(
        &handler,
        EffectKind::Completion {
            request: CompletionRequest::new("hello").into(),
            stream: false,
        },
    )
    .await
    .unwrap();
    let Answered::Completion(answer) = answer else {
        panic!("expected completion answer");
    };
    assert_eq!(answer.text(), "こんにちは！");
    server.await.unwrap();
}

/// Serves the model listing the request path asks for: Gemini's native
/// reply under `/v1beta/models`, the OpenAI-shaped one elsewhere.
async fn models_endpoint() -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0u8; 4_096];
            let Ok(read) = socket.read(&mut request).await else {
                continue;
            };
            let request = String::from_utf8_lossy(&request[..read]);
            sent.send(request.to_string()).unwrap();
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let body = if path.starts_with("/v1beta/models") {
                r#"{"models":[{"name":"models/gemini-probe"}]}"#
            } else {
                r#"{"data":[{"id":"probe-model","display_name":"Probe model"}]}"#
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _discarded = socket.write_all(response.as_bytes()).await;
        }
    });
    (address, received)
}

#[test]
fn session_secrets_are_shared_by_clones_but_isolated_between_instances() {
    let first = Credentials::new(true);
    let first_clone = first.clone();
    let second = Credentials::new(true);
    let second_clone = second.clone();
    let connection = "same-connection";

    assert_eq!(first.secret(connection).unwrap(), None);
    assert_eq!(second.secret(connection).unwrap(), None);

    save_secret(&first, connection, "first-secret").unwrap();
    assert_eq!(
        first.secret(connection).unwrap().as_deref(),
        Some("first-secret")
    );
    assert_eq!(
        first_clone.secret(connection).unwrap().as_deref(),
        Some("first-secret")
    );
    assert_eq!(second.secret(connection).unwrap(), None);
    assert_eq!(second_clone.secret(connection).unwrap(), None);

    save_secret(&second_clone, connection, "second-secret").unwrap();
    assert_eq!(
        second.secret(connection).unwrap().as_deref(),
        Some("second-secret")
    );
    assert_eq!(
        first.secret(connection).unwrap().as_deref(),
        Some("first-secret")
    );

    delete_secret(&first_clone, connection).unwrap();
    assert_eq!(first.secret(connection).unwrap(), None);
    assert_eq!(first_clone.secret(connection).unwrap(), None);
    assert_eq!(
        second.secret(connection).unwrap().as_deref(),
        Some("second-secret")
    );
    assert_eq!(
        second_clone.secret(connection).unwrap().as_deref(),
        Some("second-secret")
    );

    delete_secret(&second_clone, connection).unwrap();
    assert_eq!(second.secret(connection).unwrap(), None);
    assert_eq!(second_clone.secret(connection).unwrap(), None);
}

/// The probe lists models through the configured endpoint whichever
/// adapter names it: `openai` and `openrouter` have registry presets,
/// `gemini` maps to rig's `gcp.gemini` provider and reads its native
/// listing, and a custom adapter falls back to the OpenAI-shaped wire.
#[tokio::test]
async fn the_probe_lists_models_for_any_adapter() {
    let (address, mut requests) = models_endpoint().await;
    let credentials = Credentials::new(true);
    credentials.remember("c1", "explicit-key".into());
    for (adapter, model, path, authentication) in [
        (
            "openai",
            "probe-model",
            "/models",
            "authorization: Bearer explicit-key",
        ),
        (
            "openrouter",
            "probe-model",
            "/models",
            "authorization: Bearer explicit-key",
        ),
        (
            "anthropic",
            "probe-model",
            "/v1/models",
            "x-api-key: explicit-key",
        ),
        (
            "gemini",
            "gemini-probe",
            "/v1beta/models",
            "x-goog-api-key: explicit-key",
        ),
        (
            "custom",
            "probe-model",
            "/models",
            "authorization: Bearer explicit-key",
        ),
    ] {
        let endpoint = format!("http://{address}");
        let connection = connection_for(adapter, &endpoint);
        let check = probe(
            &credentials,
            &connection,
            &super::super::providers::Providers::none(),
        )
        .await;
        assert!(check.healthy, "{adapter}: {:?}", check.failure);
        let request = requests.recv().await.unwrap();
        let uri = request.split_whitespace().nth(1).unwrap();
        assert_eq!(uri.split('?').next().unwrap(), path, "{adapter}");
        assert!(!uri.contains("explicit-key"), "{adapter}: {uri}");
        assert!(request.contains(authentication), "{adapter}: {request}");
        assert!(
            check.resources.iter().any(|known| known == model),
            "{adapter}: {:?}",
            check.resources
        );
    }
}

#[test]
fn models_merges_configured_and_probed_resources_without_duplicates() {
    let mut conn = llm_connection("openai", "https://api.openai.com/v1", &["gpt-5.4-mini"]);
    conn.last_check = Some(Check {
        healthy: true,
        checked_at: "2026-11-19T00:00:00Z".into(),
        failure: None,
        capabilities: Vec::new(),
        resources: vec!["gpt-4o".into(), "gpt-5.4-mini".into(), "gpt-5".into()],
    });
    let ids: Vec<String> = models(&conn, Catalog::builtin())
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, vec!["gpt-5.4-mini", "gpt-4o", "gpt-5"]);
}

#[test]
fn sakura_dialect_uses_chat_route_and_permissive_embedding_quirks() {
    let providers = super::super::providers::Providers::none();
    assert!(is_sakura_connection("sakura", "https://custom.invalid/v1"));
    assert!(is_sakura_connection(
        "openai",
        "https://api.ai.sakura.ad.jp/v1/"
    ));
    assert!(!is_sakura_connection("openai", "https://api.openai.com/v1"));

    let ProviderConfig::OpenAi(cfg) = provider_config(
        "sakura",
        super::super::settings::SAKURA_AI_ENDPOINT,
        Some("tok".into()),
        &providers,
    ) else {
        panic!("expected OpenAi provider config for sakura");
    };
    assert_eq!(cfg.completion_route(), Route::Chat);
    assert!(!cfg.dialect.quirks.embedding.requires_usage);
    assert_eq!(
        cfg.dialect.quirks.embedding.dimensions,
        DimensionsField::Ignored
    );
}

#[tokio::test]
async fn sakura_probe_filters_non_chat_models_from_llm_list() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _discarded = stream.read(&mut buf).await;
        let body = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "gpt-oss-120b"},
                {"id": "whisper-large-v3-turbo"},
                {"id": "multilingual-e5-large"},
                {"id": "zundamon"},
                {"id": "preview/Qwen3.6-35B-A3B"},
            ]
        })
        .to_string();
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _discarded = stream.write_all(reply.as_bytes()).await;
    });

    let credentials = Credentials::new(true);
    credentials.remember("c1", "uuid:secret".into());
    let conn = llm_connection("sakura", &format!("http://{addr}/v1"), &[]);
    let check = probe(
        &credentials,
        &conn,
        &super::super::providers::Providers::none(),
    )
    .await;
    assert!(check.healthy, "{:?}", check.failure);
    assert_eq!(
        check.resources,
        vec!["gpt-oss-120b", "preview/Qwen3.6-35B-A3B"]
    );
}
