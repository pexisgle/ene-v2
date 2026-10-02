use super::*;
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use std::sync::{Arc, Mutex};

fn connection() -> Connection {
    Connection {
        id: "c".into(),
        kind: "llm".into(),
        adapter: ID.into(),
        endpoint: ENDPOINT.into(),
        options: json!({}),
        credential_state: "session".into(),
        capabilities: None,
        last_check: None,
    }
}

#[test]
fn builtin_policy_keeps_credentials_on_the_official_endpoint() {
    let c = connection();
    let allowed = policy(&c).unwrap();
    assert_eq!(allowed.destinations, ["https://api.openai.com"]);
    assert_eq!(
        allowed.environment_variable.as_deref(),
        Some("OPENAI_API_KEY")
    );
    for endpoint in [
        "https://openrouter.ai/api/v1",
        "https://example.com/v1",
        "http://127.0.0.1/v1",
    ] {
        let mut other = c.clone();
        other.endpoint = endpoint.into();
        assert!(policy(&other).is_err());
    }
    let mut old = c;
    old.options = json!({"only":["openai"], "allow_fallbacks":false});
    assert!(policy(&old).is_err());
}

#[tokio::test]
async fn official_responses_and_model_list_use_bearer_and_native_model_ids() {
    let received = Arc::new(Mutex::new(Vec::<Value>::new()));
    let capture = received.clone();
    let terminal = json!({"type":"response.completed", "sequence_number":2,
        "response":{"id":"resp_1", "object":"response", "created_at":0,
            "status":"completed", "model":MODEL, "output":[], "tools":[]}});
    let text = json!({"type":"response.output_text.delta", "item_id":"msg_1", "output_index":0,
        "content_index":0, "sequence_number":1, "delta":"日本🙂"});
    let sse = format!("data: {text}\n\ndata: {terminal}\n\n");
    let app = Router::new()
        .route(
            "/v1/responses",
            post(
                move |headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer fixture-key");
                    capture.lock().unwrap().push(body);
                    ([("content-type", "text/event-stream")], Body::from(sse))
                },
            ),
        )
        .route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                // OpenAI models have id/owned_by, without OpenRouter's name field.
                Json(json!({"object":"list", "data":[{"id":MODEL,"owned_by":"openai"}]}))
            }),
        )
        .route(
            "/redirect/models",
            get(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/v1/models")]) }),
        )
        .route(
            "/redirect/responses",
            post(|| async {
                (
                    StatusCode::TEMPORARY_REDIRECT,
                    [("location", "/v1/responses")],
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let request = Generation {
        endpoint: ENDPOINT.into(),
        options: json!({}),
        model: MODEL.into(),
        preamble: "system prompt".into(),
        messages: vec![
            ChatMessage {
                role: "user".into(),
                text: "previous".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                text: "reply".into(),
            },
            ChatMessage {
                role: "user".into(),
                text: "current".into(),
            },
        ],
        max_tokens: 128,
    };
    let mut events = stream(
        request.clone(),
        client("fixture-key".into(), &format!("{base}/v1")).unwrap(),
    )
    .unwrap();
    assert!(matches!(events.next().await.unwrap().unwrap(), Event::Delta(t) if t == "日本🙂"));
    assert!(matches!(events.next().await.unwrap().unwrap(), Event::Finished(r) if r == "stop"));
    assert!(events.next().await.is_none());
    let body = received.lock().unwrap()[0].clone();
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["max_output_tokens"], 128);
    assert!(body.get("provider").is_none());
    assert!(body["input"].to_string().contains("previous"));
    assert!(body["input"].to_string().contains("reply"));
    assert!(body["input"].to_string().contains("current"));
    let models = fetch_models("fixture-key".into(), &format!("{base}/v1"))
        .await
        .unwrap();
    assert_eq!(models[0].id, MODEL);
    assert_eq!(models[0].name, MODEL);
    assert!(
        models[0].metadata.is_none(),
        "official /models does not expose limits or modalities"
    );
    assert!(
        fetch_models("fixture-key".into(), &format!("{base}/redirect"))
            .await
            .is_err()
    );
    let mut redirect = stream(
        request,
        client("fixture-key".into(), &format!("{base}/redirect")).unwrap(),
    )
    .unwrap();
    assert!(redirect.next().await.unwrap().is_err());
    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "redirect must not resubmit generation"
    );
    server.abort();
}
