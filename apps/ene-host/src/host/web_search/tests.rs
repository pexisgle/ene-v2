use super::*;
use crate::host::settings::{CredentialState, Endpoint, SearchConnection};

#[test]
fn search_keeps_citable_results_and_reports_invalid_responses() {
    let body = br#"{"results":[
        {"title":"unsafe","url":"javascript:alert(1)","content":"ignored"},
        {"title":"Rust","url":"https://www.rust-lang.org/","content":"Official site"},
        {"title":"Other","url":"https://example.org/","content":"beyond the limit"}
    ]}"#;
    let output = parse_adapter_results(SearchAdapter::Mwmbl, "Mwmbl", body, 1, None).unwrap();
    assert_eq!(
        output["results"],
        json!([{"title":"Rust","url":"https://www.rust-lang.org/","snippet":"Official site"}])
    );
    assert_eq!(
        parse_adapter_results(SearchAdapter::Mwmbl, "Mwmbl", br#"{"results":[]}"#, 5, None)
            .unwrap()["results"],
        json!([])
    );
    assert!(
        parse_adapter_results(
            SearchAdapter::Mwmbl,
            "Mwmbl",
            b"service unavailable",
            5,
            None
        )
        .is_err()
    );
}

fn search_test_connection(
    id: &str,
    name: &str,
    adapter: &str,
    endpoint: String,
    credential_state: CredentialState,
    max_results: u32,
) -> Connection {
    Connection {
        id: ene_domain::ConnectionId::parse(id).unwrap(),
        name: name.into(),
        enabled: true,
        kind: ConnectionKind::Search,
        credential_state,
        revision: 1,
        last_check: None,
        config: ConnectionConfig::Search(SearchConnection {
            endpoint: Endpoint {
                adapter: adapter.into(),
                endpoint,
                ..Default::default()
            },
            max_results,
        }),
    }
}

async fn spawn_mock_search_adapters() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    let app = Router::new()
        .route(
            "/mwmbl",
            get(|| async { http::StatusCode::MOVED_PERMANENTLY }),
        )
        .route(
            "/mwmbl/",
            get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                assert_eq!(q.get("q").map(String::as_str), Some("rust"));
                Json(json!({"results":[{"url":"https://mwmbl.example/1","title":"Mwmbl Hit","content":"mwmbl ok"}]}))
            }),
        )
        .route(
            "/searxng/search",
            get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                assert_eq!(q.get("format").map(String::as_str), Some("json"));
                Json(json!({"results":[{"url":"https://searx.example/1","title":"SearX Hit","content":"ok"}]}))
            }),
        )
        .route(
            "/brave",
            get(|headers: http::HeaderMap| async move {
                assert_eq!(headers["x-subscription-token"], "brave-secret");
                Json(json!({"web":{"results":[{"url":"https://brave.example/1","title":"Brave Hit","description":"brave desc"}]}}))
            }),
        )
        .route(
            "/tavily",
            post(|headers: http::HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers[http::header::AUTHORIZATION], "Bearer tavily-secret");
                assert_eq!(body["api_key"], "tavily-secret");
                assert_eq!(body["query"], "rust");
                Json(json!({"results":[{"url":"https://tavily.example/1","title":"Tavily Hit","content":"tavily desc"}]}))
            }),
        )
        .route(
            "/exa",
            post(|headers: http::HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["x-api-key"], "exa-secret");
                assert_eq!(body["query"], "rust");
                assert_eq!(body["numResults"], 5);
                assert_eq!(body["type"], "auto");
                assert_eq!(body["contents"]["highlights"]["maxCharacters"], 500);
                Json(json!({
                    "results": [
                        {
                            "url": "https://exa.example/?token=exa%2Dsecret",
                            "title": "a result that reflects the credential"
                        },
                        {
                            "url": "https://exa.example/1",
                            "title": "Exa Hit",
                            "highlights": ["exa highlight 1", "exa highlight 2"]
                        },
                        {
                            "url": "https://exa.example/2",
                            "title": "Exa Fallback",
                            "text": "exa text fallback"
                        }
                    ]
                }))
            }),
        )
        .route(
            "/exa-unauthorized",
            post(|| async {
                (
                    http::StatusCode::UNAUTHORIZED,
                    Json(json!({"error": "Rejected API key: wrong-key"})),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, server)
}

#[tokio::test]
async fn f13_configured_search_adapters_execute_and_respect_credentials() {
    let (addr, server) = spawn_mock_search_adapters().await;

    let creds = Credentials::new(true);
    creds.remember("s-brave", "brave-secret".into());
    creds.remember("s-tavily", "  tavily-secret \n".into());
    creds.remember("s-exa", "exa-secret ".into());
    creds.remember("s-exa-bad", "wrong-key".into());
    let providers = Providers::none();
    let throttle = Arc::new(Mutex::new(None));

    // Mwmbl must keep its trailing slash even though endpoint_of trims it.
    let mwmbl_conn = search_test_connection(
        "s-mwmbl",
        "Mwmbl",
        "mwmbl",
        format!("http://{addr}/mwmbl/"),
        CredentialState::Absent,
        5,
    );
    let resolved_mwmbl = resolve_connection(&mwmbl_conn, Some(&creds), &providers, 0).unwrap();
    assert!(resolved_mwmbl.binding.endpoint.path().ends_with('/'));
    let out = execute(&resolved_mwmbl, "rust", 5, &throttle)
        .await
        .unwrap();
    assert_eq!(out["results"][0]["title"], "Mwmbl Hit");

    let searx_conn = search_test_connection(
        "s-searx",
        "Local SearXNG",
        "searxng",
        format!("http://{addr}/searxng"),
        CredentialState::Absent,
        3,
    );
    let resolved_searx = resolve_connection(&searx_conn, Some(&creds), &providers, 0).unwrap();
    let out = execute(&resolved_searx, "rust", 5, &throttle)
        .await
        .unwrap();
    assert_eq!(out["results"][0]["title"], "SearX Hit");

    let brave_conn = search_test_connection(
        "s-brave",
        "Brave",
        "brave",
        format!("http://{addr}/brave"),
        CredentialState::Registered,
        5,
    );
    let resolved_brave = resolve_connection(&brave_conn, Some(&creds), &providers, 0).unwrap();
    let out = execute(&resolved_brave, "rust", 5, &throttle)
        .await
        .unwrap();
    assert_eq!(out["results"][0]["snippet"], "brave desc");

    let tavily_conn = search_test_connection(
        "s-tavily",
        "Tavily",
        "tavily",
        format!("http://{addr}/tavily"),
        CredentialState::Registered,
        5,
    );
    let resolved_tavily = resolve_connection(&tavily_conn, Some(&creds), &providers, 0).unwrap();
    let out = execute(&resolved_tavily, "rust", 5, &throttle)
        .await
        .unwrap();
    assert_eq!(out["results"][0]["title"], "Tavily Hit");

    let exa_conn = search_test_connection(
        "s-exa",
        "Exa",
        "exa",
        format!("http://{addr}/exa"),
        CredentialState::Registered,
        5,
    );
    assert_eq!(
        crate::host::settings::environment_variable(&exa_conn, &providers).as_deref(),
        Some("EXA_API_KEY")
    );
    assert_eq!(
        crate::host::settings::environment_variable(&tavily_conn, &providers).as_deref(),
        Some("TAVILY_API_KEY")
    );
    let resolved_exa = resolve_connection(&exa_conn, Some(&creds), &providers, 0).unwrap();
    let out = execute(&resolved_exa, "rust", 5, &throttle).await.unwrap();
    assert_eq!(out["results"].as_array().unwrap().len(), 2);
    assert!(!out.to_string().contains("exa%2Dsecret"));
    assert_eq!(out["results"][0]["title"], "Exa Hit");
    assert_eq!(
        out["results"][0]["snippet"],
        "exa highlight 1 ... exa highlight 2"
    );
    assert_eq!(out["results"][1]["snippet"], "exa text fallback");

    // Missing Exa credential refuses cleanly.
    let exa_no_key = Connection {
        id: ene_domain::ConnectionId::parse("s-exa-missing").unwrap(),
        ..exa_conn.clone()
    };
    let missing_err = resolve_connection(&exa_no_key, Some(&creds), &providers, 0)
        .err()
        .expect("missing Exa credential must fail")
        .to_string();
    assert!(missing_err.contains("requires an API key credential"));

    // Report the status without forwarding reflected credentials.
    let exa_bad = Connection {
        id: ene_domain::ConnectionId::parse("s-exa-bad").unwrap(),
        config: ConnectionConfig::Search(SearchConnection {
            endpoint: Endpoint {
                adapter: "exa".into(),
                endpoint: format!("http://{addr}/exa-unauthorized"),
                ..Default::default()
            },
            max_results: 5,
        }),
        ..exa_conn
    };
    let resolved_bad = resolve_connection(&exa_bad, Some(&creds), &providers, 0).unwrap();
    let err = execute(&resolved_bad, "rust", 5, &throttle)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("HTTP 401"));
    assert!(!err.to_string().contains("wrong-key"));

    server.abort();
}

#[test]
fn resolve_skips_unconfigured_enabled_search_when_another_is_ready() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
    let creds = Credentials::new(true);
    creds.remember("search-exa", "exa-valid-key".into());
    let providers = Providers::none();

    let rev = store.settings_repo().settings_revision().unwrap();
    // `a-unconfigured-tavily` sorts before `search-exa` by id, but has no key.
    let unconfigured = Connection {
        id: ene_domain::ConnectionId::parse("a-unconfigured-tavily").unwrap(),
        name: "Tavily (no key)".into(),
        enabled: true,
        kind: ConnectionKind::Search,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Search(SearchConnection {
            endpoint: Endpoint {
                adapter: "tavily".into(),
                endpoint: SearchAdapter::Tavily
                    .provider()
                    .default_endpoint
                    .unwrap()
                    .into(),
                ..Default::default()
            },
            max_results: 5,
        }),
    };
    let ready_exa = Connection {
        id: ene_domain::ConnectionId::parse("search-exa").unwrap(),
        name: "Web検索 (Exa)".into(),
        enabled: true,
        kind: ConnectionKind::Search,
        credential_state: CredentialState::Registered,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Search(SearchConnection {
            endpoint: Endpoint {
                adapter: "exa".into(),
                endpoint: SearchAdapter::Exa
                    .provider()
                    .default_endpoint
                    .unwrap()
                    .into(),
                ..Default::default()
            },
            max_results: 5,
        }),
    };
    store
        .settings_repo()
        .update_settings(rev, &[(unconfigured, 0), (ready_exa, 0)])
        .unwrap();

    let resolved = resolve(&store, Some(&creds), &providers).unwrap();
    assert_eq!(resolved.adapter, SearchAdapter::Exa);
    assert_eq!(
        resolved.binding.endpoint.as_str(),
        SearchAdapter::Exa.provider().default_endpoint.unwrap()
    );
}

fn search_result_contains<A: bevy_ecs::prelude::Component>(
    world: &mut bevy_ecs::prelude::World,
    expected: &str,
    expect_error: bool,
) -> bool {
    use bevy_ecs::prelude::With;
    use rig_core::completion::Message;
    use rig_core::message::{ToolResultContent, UserContent};
    use rig_ecs::prelude::Conversation;
    world
        .query_filtered::<&Conversation, With<A>>()
        .iter(world)
        .any(|conversation| {
            conversation.entries().any(|entry| match &entry.message {
                Message::User { content } => content.iter().any(|item| {
                    matches!(item, UserContent::ToolResult(result)
                    if result.name.as_str() == NAME
                        && result.is_error == expect_error
                        && result.content.iter().any(|block| match block {
                            ToolResultContent::Text(text) => text.text.contains(expected),
                            ToolResultContent::Json { value } => value.to_string().contains(expected),
                            _ => false,
                        }))
                }),
                _ => false,
            })
        })
}

fn companion_replied_with(world: &mut bevy_ecs::prelude::World, expected: &str) -> bool {
    use bevy_ecs::prelude::With;
    use rig_core::completion::Message;
    use rig_ecs::prelude::Conversation;
    world
        .query_filtered::<&Conversation, With<crate::host::agent::CompanionAgent>>()
        .iter(world)
        .any(|conversation| {
            conversation.entries().any(|entry| {
                matches!(&entry.message, Message::Assistant(reply)
                    if reply.text().contains(expected))
            })
        })
}

async fn spawn_searx_test_server(
    hits: Arc<std::sync::atomic::AtomicUsize>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::get};
    use std::sync::atomic::Ordering;
    let app = Router::new().route(
        "/searx/search",
        get(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            async {
                Json(json!({"results":[{"url":"https://ok.example/","title":"Configured Hit","content":"verified"}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, server)
}

async fn spawn_exa_tavily_test_server(
    exa_hits: Arc<std::sync::atomic::AtomicUsize>,
    tavily_hits: Arc<std::sync::atomic::AtomicUsize>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use axum::{Json, Router, routing::post};
    use std::sync::atomic::Ordering;
    let app = Router::new()
        .route(
            "/exa",
            post(move |headers: http::HeaderMap, Json(body): Json<Value>| {
                let exa_counter = Arc::clone(&exa_hits);
                async move {
                    assert_eq!(headers["x-api-key"], "exa-live-key");
                    assert!(body["query"].as_str().is_some_and(|q| !q.is_empty()));
                    exa_counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "results": [{
                            "url": "https://exa.ai/docs",
                            "title": "Exa Live Hit",
                            "highlights": ["Exa semantic snippet"]
                        }]
                    }))
                }
            }),
        )
        .route(
            "/tavily",
            post(move |headers: http::HeaderMap, Json(body): Json<Value>| {
                let tavily_counter = Arc::clone(&tavily_hits);
                async move {
                    assert_eq!(
                        headers[http::header::AUTHORIZATION],
                        "Bearer tavily-live-key"
                    );
                    assert_eq!(body["api_key"], "tavily-live-key");
                    assert!(body["query"].as_str().is_some_and(|q| !q.is_empty()));
                    tavily_counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "results": [{
                            "url": "https://tavily.com/docs",
                            "title": "Tavily Live Hit",
                            "content": "Tavily snippet"
                        }]
                    }))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, server)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f13_b04_unconfigured_search_refuses_without_http_and_gates_on_configured_host() {
    use crate::host::tests::Harness;
    use rig_core::test_utils::MockStreamEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let (addr, server) = spawn_searx_test_server(Arc::clone(&hits)).await;

    // Part 1: no Search connection configured -> Gate refuses before approval or HTTP.
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("call-1", NAME, json!({"query": "rust"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("検索接続が未設定です"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("了解"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::tool_call("call-2", NAME, json!({"query": "rust"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("見つかりました"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("完了"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let companion_id = harness.companion.clone();
    crate::host::work::start(
        harness.app.world_mut(),
        &ene_rpc::StartWorkRequest {
            companion_id: companion_id.clone(),
            instruction: "検索して".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        search_result_contains::<crate::host::agent::WorkAgent>(world, "search_unconfigured", true)
            && companion_replied_with(world, "了解")
    });
    assert!(
        harness
            .store()
            .approval_repo()
            .approvals(crate::host::permission::ApprovalStatus::Pending)
            .unwrap()
            .is_empty(),
        "unconfigured web_search must fail before creating an approval card"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    // Part 2: configuring Search connection + Allow policy for "web_search" executes against configured URL.
    let rev = harness.store().settings_repo().settings_revision().unwrap();
    let search_conn = Connection {
        revision: 0,
        ..search_test_connection(
            "search-1",
            "Local SearXNG",
            "searxng",
            format!("http://{addr}/searx"),
            CredentialState::Absent,
            5,
        )
    };
    harness
        .store()
        .settings_repo()
        .update_settings(rev, &[(search_conn, 0)])
        .unwrap();
    harness
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[crate::host::permission::PolicyRule {
                id: "allow-search".into(),
                tool_name: NAME.into(),
                pattern: "*".into(),
                effect: crate::host::permission::PolicyEffect::Allow,
                created_at: util::now(),
            }],
            &[],
        )
        .unwrap();
    crate::host::work::start(
        harness.app.world_mut(),
        &ene_rpc::StartWorkRequest {
            companion_id,
            instruction: "再検索して".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        search_result_contains::<crate::host::agent::WorkAgent>(world, "Configured Hit", false)
    });
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

fn configure_exa_tavily_harness() -> crate::host::tests::Harness {
    use crate::host::tests::Harness;
    use rig_core::test_utils::MockStreamEvent;
    let harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("call-exa", NAME, json!({"query": "rust exa"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Exaで見つかりました"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Exa完了"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::tool_call("call-tavily", NAME, json!({"query": "rust tavily"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Tavilyで見つかりました"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("Tavily完了"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    let creds = harness.app.world().resource::<Credentials>().clone();
    creds.remember("search-exa", "exa-live-key".into());
    creds.remember("search-tavily", "tavily-live-key".into());
    harness
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[crate::host::permission::PolicyRule {
                id: "allow-web-search".into(),
                tool_name: NAME.into(),
                pattern: "*".into(),
                effect: crate::host::permission::PolicyEffect::Allow,
                created_at: util::now(),
            }],
            &[],
        )
        .unwrap();
    harness
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exa_and_tavily_work_end_to_end_and_switch_cleanly_in_work_agent() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let exa_hits = Arc::new(AtomicUsize::new(0));
    let tavily_hits = Arc::new(AtomicUsize::new(0));
    let (addr, server) =
        spawn_exa_tavily_test_server(Arc::clone(&exa_hits), Arc::clone(&tavily_hits)).await;

    let mut harness = configure_exa_tavily_harness();
    let companion_id = harness.companion.clone();

    let exa_conn = Connection {
        revision: 0,
        ..search_test_connection(
            "search-exa",
            "Web検索 (Exa)",
            "exa",
            format!("http://{addr}/exa"),
            CredentialState::Registered,
            5,
        )
    };
    let tavily_conn = Connection {
        enabled: false,
        revision: 0,
        ..search_test_connection(
            "search-tavily",
            "Web検索 (Tavily)",
            "tavily",
            format!("http://{addr}/tavily"),
            CredentialState::Registered,
            5,
        )
    };
    let rev = harness.store().settings_repo().settings_revision().unwrap();
    harness
        .store()
        .settings_repo()
        .update_settings(rev, &[(exa_conn.clone(), 0), (tavily_conn.clone(), 0)])
        .unwrap();
    let probe_creds = harness.app.world().resource::<Credentials>().clone();
    assert!(
        probe(&exa_conn, &probe_creds, &Providers::none())
            .await
            .healthy
    );
    assert!(
        probe(&tavily_conn, &probe_creds, &Providers::none())
            .await
            .healthy
    );

    crate::host::work::start(
        harness.app.world_mut(),
        &ene_rpc::StartWorkRequest {
            companion_id: companion_id.clone(),
            instruction: "Exaで検索して".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        search_result_contains::<crate::host::agent::WorkAgent>(world, "Exa Live Hit", false)
            && companion_replied_with(world, "Exa完了")
    });

    // 2. Switch active search to Tavily and verify WorkAgent uses Tavily.
    let settings = harness.store().settings_repo().settings().unwrap();
    let exa_off = Connection {
        enabled: false,
        ..exa_conn
    };
    let tavily_on = Connection {
        enabled: true,
        ..tavily_conn
    };
    harness
        .store()
        .settings_repo()
        .update_settings(settings.revision, &[(exa_off, 1), (tavily_on, 1)])
        .unwrap();
    crate::host::work::start(
        harness.app.world_mut(),
        &ene_rpc::StartWorkRequest {
            companion_id,
            instruction: "Tavilyで検索して".into(),
            ..Default::default()
        },
    )
    .unwrap();
    harness.run_until(|world| {
        search_result_contains::<crate::host::agent::WorkAgent>(world, "Tavily Live Hit", false)
    });

    // 1 probe + 1 WorkAgent search each = 2 hits per provider.
    assert_eq!(exa_hits.load(Ordering::SeqCst), 2);
    assert_eq!(tavily_hits.load(Ordering::SeqCst), 2);
    server.abort();
}

#[test]
fn adapters_parse_their_config_spellings_and_label_themselves() {
    // The parse key is what a saved `Search` connection stores; the label is
    // what error text and the result header show. Both are pinned here so a
    // derive change cannot silently swap one for the other.
    for (adapter, key, label) in [
        (SearchAdapter::Mwmbl, "mwmbl", "Mwmbl"),
        (SearchAdapter::Searxng, "searxng", "SearXNG"),
        (SearchAdapter::Brave, "brave", "Brave"),
        (SearchAdapter::Tavily, "tavily", "Tavily"),
        (SearchAdapter::Exa, "exa", "Exa"),
    ] {
        assert_eq!(adapter.provider().name, label);
        assert_eq!(SearchAdapter::parse(key), Some(adapter));
        // The old parser trimmed and lowercased, so padded or upper case
        // configuration still resolved.
        assert_eq!(
            SearchAdapter::parse(&format!(" {} ", key.to_uppercase())),
            Some(adapter)
        );
    }
    assert_eq!(SearchAdapter::parse("bing"), None);
    assert_eq!(SearchAdapter::parse(""), None);
}

#[test]
fn the_tool_arguments_schema_is_derived_without_drift() {
    // Pinned because this schema is the LLM tool-calling contract: the model
    // sees exactly these keys and this required set.
    assert_eq!(
        tool::argument_schema(),
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_CHARS},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS},
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    );
}

fn configure_test_search(harness: &crate::host::tests::Harness, endpoint: String) {
    let store = harness.store();
    let repo = store.settings_repo();
    let settings = repo.settings().unwrap();
    let revision = settings
        .connections
        .iter()
        .find(|connection| connection.id.as_str() == "search-1")
        .map_or(0, |connection| connection.revision);
    let connection = search_test_connection(
        "search-1",
        "Local Search",
        "searxng",
        endpoint,
        CredentialState::Absent,
        5,
    );
    repo.update_settings(settings.revision, &[(connection, revision)])
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_companion_answers_a_quick_search_in_its_own_turn() {
    use crate::host::{agent::CompanionAgent, tests::Harness};
    use rig_core::test_utils::MockStreamEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let hits = Arc::new(AtomicUsize::new(0));
    let (address, server) = spawn_searx_test_server(Arc::clone(&hits)).await;
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("call", NAME, json!({"query": "ene"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("出典は https://ok.example/ です"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    configure_test_search(&harness, format!("http://{address}/searx"));
    harness
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[ene_policy::PolicyRule {
                id: "allow-fixture".into(),
                tool_name: NAME.into(),
                pattern: format!("http://{address}/searx/search"),
                effect: ene_policy::PolicyEffect::Allow,
                created_at: util::now(),
            }],
            &[],
        )
        .unwrap();
    harness.accept("search", "今すぐ調べて").unwrap();
    harness.run_until(|world| companion_replied_with(world, "出典は https://ok.example/ です"));
    assert!(search_result_contains::<CompanionAgent>(
        harness.app.world_mut(),
        "https://ok.example/",
        false,
    ));
    let world = harness.app.world_mut();
    assert!(
        world
            .query::<&crate::host::agent::WorkAgent>()
            .iter(world)
            .next()
            .is_none()
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_search_cannot_send_the_query_after_its_connection_changes() {
    use crate::host::{agent::CompanionAgent, permission, tests::Harness};
    use rig_core::test_utils::MockStreamEvent;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let original_hits = Arc::new(AtomicUsize::new(0));
    let replacement_hits = Arc::new(AtomicUsize::new(0));
    let (original, original_server) = spawn_searx_test_server(Arc::clone(&original_hits)).await;
    let (replacement, replacement_server) =
        spawn_searx_test_server(Arc::clone(&replacement_hits)).await;
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("call", NAME, json!({"query": "ene"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("検索接続が変わりました"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    configure_test_search(&harness, format!("http://{original}/searx"));
    harness
        .store()
        .policy_repo()
        .apply_policy_rules(
            &[ene_policy::PolicyRule {
                id: "confirm-fixture".into(),
                tool_name: NAME.into(),
                pattern: "*".into(),
                effect: ene_policy::PolicyEffect::Confirm,
                created_at: util::now(),
            }],
            &[],
        )
        .unwrap();
    harness.accept("search", "今すぐ調べて").unwrap();
    let store = harness.store();
    let approvals = || {
        store
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
    };
    harness.run_until(|_| approvals().len() == 1);
    assert_eq!(original_hits.load(Ordering::SeqCst), 0);
    let request_id = approvals()[0].request_id.clone();
    let companion = harness.companion.clone();
    assert!(
        permission::submit_with_outcome(
            harness.app.world_mut(),
            permission::ApprovalRequest::DecisionModel {
                request: &request_id,
                companion: &companion,
            },
            permission::ApprovalDecision::Approve,
        )
        .is_err()
    );
    assert_eq!(approvals().len(), 1);
    configure_test_search(&harness, format!("http://{replacement}/searx"));
    let error = permission::submit_with_outcome(
        harness.app.world_mut(),
        permission::ApprovalRequest::Operator(&request_id),
        permission::ApprovalDecision::AlwaysApprove,
    )
    .err()
    .expect("the approval must expire if the search destination changed");
    assert!(error.to_string().contains("search connection changed"));
    assert_eq!(
        store
            .approval_repo()
            .approval(&request_id)
            .unwrap()
            .unwrap()
            .status,
        permission::ApprovalStatus::Expired
    );
    assert_eq!(store.policy_repo().policy_rules().unwrap().len(), 1);
    harness.run_until(|world| {
        search_result_contains::<CompanionAgent>(world, "search connection changed", true)
    });
    assert_eq!(original_hits.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_hits.load(Ordering::SeqCst), 0);
    original_server.abort();
    replacement_server.abort();
}
