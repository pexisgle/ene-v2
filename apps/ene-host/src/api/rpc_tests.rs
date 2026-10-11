use super::*;
use crate::infrastructure::sqlite::Store;
use ene_api_client::{ApiClient, Error};
use ene_core::ports::{ConfigurationCommit, Repository};
use ene_rpc as w;
use serde_json::json;
use tokio::time::{Duration, timeout};

struct Host {
    app: App,
    client: ApiClient,
    url: String,
    server: tokio::task::JoinHandle<()>,
    temp: tempfile::TempDir,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.app.shutdown.send_replace(true);
        self.server.abort();
    }
}
impl Host {
    async fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("host.db")).await.unwrap();
        let app = crate::bootstrap::application(store, "private-token".into());
        Self::serve(app, temp).await
    }
    async fn serve(mut app: App, temp: tempfile::TempDir) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let client = ApiClient::new(&ene_api_client::RuntimeConnection {
            endpoint: format!("{url}/rpc"),
            token: "private-token".into(),
        })
        .unwrap();
        app.web_origin = Some(url.clone().into());
        let router = router(app.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            app,
            client,
            url,
            server,
            temp,
        }
    }
    async fn device(&self, caps: &[&str]) -> (w::Device, ApiClient) {
        let client = self.client.device_rpc();
        let registration = client
            .register_device(w::RegisterDeviceRequest {
                name: "fixture".into(),
                capabilities: caps.iter().map(|s| (*s).into()).collect(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_owned();
        let d = registration.device.into_option().unwrap();
        client
            .approve(w::Id {
                id: d.id.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        for cap in caps {
            client
                .set_permission(w::DevicePermissionRequest {
                    device_id: d.id.clone(),
                    capability: (*cap).into(),
                    allowed: true,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let connected = ApiClient::new(&ene_api_client::RuntimeConnection {
            endpoint: format!("{}/rpc", self.url),
            token: registration.token,
        })
        .unwrap();
        (d, connected)
    }
}
#[tokio::test]
async fn connect_unary_streaming_and_sdk_share_a_committed_snapshot() {
    let host = Host::new().await;
    let mut events = host.client.subscribe().await.unwrap();
    let subscription = events.ready().connection_id.clone();
    let snapshot = host.client.snapshot(Some(&subscription)).await.unwrap();
    let c = &snapshot.companions[0];
    let updated = host
        .client
        .patch_companion(
            &c.id,
            &w::UpdateCompanionRequest {
                revision: c.revision,
                name: Some("Connect日本語".into()),
                mind: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let event = timeout(Duration::from_secs(2), events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let ene_api_client::ServerEvent::Notification(event) = event else {
        panic!("expected notification");
    };
    assert!(matches!(
        event.event,
        Some(w::notification::Event::CompanionUpdated(_))
    ));
    assert_eq!(event.cursor.seq, snapshot.cursor.seq + 1);
    assert_eq!(
        {
            let Some(w::notification::Event::CompanionUpdated(companion)) = event.event else {
                panic!("companion event missing")
            };
            companion.name
        },
        updated.name
    );
    let conflict = host
        .client
        .patch_companion(
            &c.id,
            &w::UpdateCompanionRequest {
                revision: c.revision,
                name: Some("stale".into()),
                mind: None,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    let Error::Api(failure) = conflict else {
        panic!("expected typed error detail");
    };
    assert_eq!(failure.status, 409);
    assert_eq!(failure.body.error.code, "revision_conflict");
    assert!(!failure.body.request_id.is_empty());
    events.close().await.unwrap();
    timeout(Duration::from_secs(2), async {
        while host
            .app
            .application
            .snapshot(Some(&subscription))
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let session = host.client.synced_session().await.unwrap();
    assert_eq!(session.snapshot().companions[0].name, updated.name);
    let wrong = ApiClient::new(&ene_api_client::RuntimeConnection {
        endpoint: format!("{}/rpc", host.url),
        token: "wrong".into(),
    })
    .unwrap();
    assert!(matches!(wrong.settings().await, Err(Error::Unauthorized)));
    assert!(matches!(wrong.subscribe().await, Err(Error::Unauthorized)));
}
#[tokio::test]
async fn updates_during_subscribe_are_either_in_snapshot_or_following_events() {
    let host = Host::new().await;
    for index in 0..12 {
        let companion = host.client.companions().await.unwrap().remove(0);
        let name = format!("update-{index}");
        let input = w::UpdateCompanionRequest {
            revision: companion.revision,
            name: Some(name.clone()),
            mind: None,
            ..Default::default()
        };
        let (stream, update) = tokio::join!(
            host.client.subscribe(),
            host.client.patch_companion(&companion.id, &input)
        );
        update.unwrap();
        let mut stream = stream.unwrap();
        let snapshot = stream.take_initial_snapshot().unwrap();
        if snapshot.companions[0].name != name {
            let Some(ene_api_client::ServerEvent::Notification(event)) =
                timeout(Duration::from_secs(2), stream.next())
                    .await
                    .unwrap()
                    .unwrap()
            else {
                panic!("update lost");
            };
            assert_eq!(event.cursor.seq, snapshot.cursor.seq + 1);
            assert_eq!(
                {
                    let Some(w::notification::Event::CompanionUpdated(companion)) = event.event
                    else {
                        panic!("companion event missing")
                    };
                    companion.name
                },
                name
            );
        }
        stream.close().await.unwrap();
    }
}
#[tokio::test]
async fn device_operations_are_scoped_durable_and_never_requeued() {
    let host = Host::new().await;
    let (device, client) = host.device(&["tools.test"]).await;
    let controller = host.client.device_rpc();
    let native = client.device_rpc();
    assert!(matches!(client.settings().await, Err(Error::Api(error)) if error.status==403));
    let mut stream = native
        .subscribe(w::Id {
            id: device.id.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    let ready = stream
        .message::<w::DeviceEvent>()
        .await
        .unwrap()
        .unwrap()
        .to_owned_message();
    assert!(matches!(
        ready.event,
        Some(w::device_event::Event::Ready(_))
    ));
    let request = w::DeviceOperationRequest {
        device_id: device.id.clone(),
        operation_id: uuid::Uuid::new_v4().to_string(),
        capability: "tools.test".into(),
        input: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    controller.request_operation(request.clone()).await.unwrap();
    let command = stream
        .message::<w::DeviceEvent>()
        .await
        .unwrap()
        .unwrap()
        .to_owned_message();
    let Some(w::device_event::Event::Operation(command)) = command.event else {
        panic!("expected operation");
    };
    let result = w::DeviceOperationResult {
        device_id: device.id.clone(),
        operation_id: request.operation_id.clone(),
        subscription_id: command.subscription_id,
        success: true,
        output: w::JsonDocument {
            json: "{}".into(),
            ..Default::default()
        }
        .into(),
        ..Default::default()
    };
    let (_, other_client) = host.device(&["tools.test"]).await;
    assert_eq!(
        other_client
            .device_rpc()
            .complete_operation(result.clone())
            .await
            .err()
            .unwrap()
            .code,
        connectrpc::ErrorCode::PermissionDenied
    );
    let completed = native
        .complete_operation(result.clone())
        .await
        .unwrap()
        .into_owned();
    assert_eq!(completed.status, w::OperationStatus::Completed);
    assert_eq!(
        native
            .complete_operation(result)
            .await
            .unwrap()
            .into_owned(),
        completed
    );
    assert_eq!(
        controller
            .request_operation(request.clone())
            .await
            .unwrap()
            .into_owned(),
        completed
    );
    let mut uncertain = request.clone();
    uncertain.operation_id = uuid::Uuid::new_v4().to_string();
    controller
        .request_operation(uncertain.clone())
        .await
        .unwrap();
    drop(stream);
    timeout(Duration::from_secs(2), async {
        loop {
            if host
                .app
                .devices
                .operation(&device.id, &uncertain.operation_id)
                .await
                .unwrap()
                .status
                == w::OperationStatus::Uncertain
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut fresh = native
        .subscribe(w::Id {
            id: device.id.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    fresh.message::<w::DeviceEvent>().await.unwrap();
    assert_eq!(
        controller
            .request_operation(uncertain.clone())
            .await
            .unwrap()
            .into_owned()
            .status,
        w::OperationStatus::Uncertain
    );
    assert!(
        timeout(Duration::from_millis(50), fresh.message::<w::DeviceEvent>())
            .await
            .is_err()
    );
    let store = Store::open(host.temp.path().join("host.db")).await.unwrap();
    let recovered = crate::devices::Devices::new(Some(store));
    assert_eq!(
        recovered
            .operation(&device.id, &uncertain.operation_id)
            .await
            .unwrap()
            .status,
        w::OperationStatus::Uncertain
    );
    controller
        .revoke(w::Id {
            id: device.id,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(fresh.message::<w::DeviceEvent>().await.unwrap().is_none());
    assert!(
        matches!(client.device_rpc().list(w::Empty::default()).await,Err(error) if error.code==connectrpc::ErrorCode::Unauthenticated)
    );
}
#[tokio::test]
async fn browser_sessions_enforce_origin_and_logout_ends_streams() {
    let host = Host::new().await;
    let http = reqwest_client();
    for path in ["/", "/app.js", "/style.css"] {
        assert_eq!(
            http.get(format!("{}{path}", host.url))
                .header("Authorization", "Bearer private-token")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    assert_eq!(
        http.post(format!("{}/session", host.url))
            .header("Authorization", "Bearer private-token")
            .header("Origin", "https://foreign.test")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let response = http
        .post(format!("{}/session", host.url))
        .header("Authorization", "Bearer private-token")
        .header("Origin", &host.url)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let set_cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    let cookie = set_cookie.split(';').next().unwrap();
    let transport = connectrpc::client::HttpClient::plaintext();
    let config =
        connectrpc::client::ClientConfig::new(format!("{}/rpc", host.url).parse().unwrap())
            .with_default_header("origin", host.url.as_str())
            .with_default_header("cookie", cookie);
    let browser = w::HostServiceClient::new(transport, config);
    let mut stream = browser.subscribe(w::Empty::default()).await.unwrap();
    assert!(stream.message::<w::HostEvent>().await.unwrap().is_some());
    assert_eq!(
        http.delete(format!("{}/session", host.url))
            .header("Cookie", cookie)
            .header("Origin", &host.url)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert!(
        timeout(Duration::from_secs(2), stream.message::<w::HostEvent>())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        browser
            .get_settings(w::Empty::default())
            .await
            .err()
            .unwrap()
            .code,
        connectrpc::ErrorCode::Unauthenticated
    );
}
fn reqwest_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn document(value: serde_json::Value) -> w::JsonDocument {
    w::JsonDocument {
        json: value.to_string(),
        ..Default::default()
    }
}
fn set_document(value: serde_json::Value) -> w::JsonDocumentPatch {
    w::JsonDocumentPatch {
        change: Some(w::json_document_patch::Change::Set(Box::new(document(
            value,
        )))),
        ..Default::default()
    }
}
fn clear_document() -> w::JsonDocumentPatch {
    w::JsonDocumentPatch {
        change: Some(w::json_document_patch::Change::Clear(Box::default())),
        ..Default::default()
    }
}

#[tokio::test]
async fn omitted_settings_preserve_connections_credentials_grants_and_notifications() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let mut config = store.configuration().await.unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    config.settings.connections = vec![w::Connection {
        id: id.clone(),
        kind: "llm".into(),
        adapter: "openai".into(),
        endpoint: "https://api.openai.com/v1".into(),
        options: document(json!({})).into(),
        credential_state: "available".into(),
        ..Default::default()
    }];
    config
        .credential_refs
        .insert(id.clone(), "fixture-reference".into());
    let companion = config.companions.values_mut().next().unwrap();
    companion
        .model_assignments
        .get_or_insert_default()
        .companion = w::ModelAssignment {
        connection_id: id.clone(),
        model_id: "model".into(),
        ..Default::default()
    }
    .into();
    config.grants.push(ene_core::domain::Grant {
        id: uuid::Uuid::new_v4().to_string(),
        companion_id: companion.id.clone(),
        kind: "llm".into(),
        connection_id: id,
        destinations: vec!["https://api.openai.com".into()],
        data_kinds: vec!["messages".into()],
        purpose: ene_core::domain::GrantPurpose::Conversation,
        status: ene_core::domain::GrantStatus::Active,
        revision: 1,
        invalidation_reason: None,
        connection_fingerprint: json!({}),
    });
    store
        .commit_configuration(ConfigurationCommit::Update(config.clone()))
        .await
        .unwrap();
    let host = Host::serve(
        crate::bootstrap::application(store.clone(), "private-token".into()),
        temp,
    )
    .await;
    let mut events = host.client.subscribe().await.unwrap();
    let before = host.client.snapshot(None).await.unwrap();
    let unchanged = host
        .client
        .patch_settings(&w::UpdateSettingsRequest {
            revision: before.settings.revision,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(unchanged, *before.settings);
    assert_eq!(
        serde_json::to_value(store.configuration().await.unwrap()).unwrap(),
        serde_json::to_value(config).unwrap()
    );
    assert_eq!(
        host.client.snapshot(None).await.unwrap().cursor,
        before.cursor
    );
    assert!(
        timeout(Duration::from_millis(50), events.next())
            .await
            .is_err()
    );
    let error = host
        .client
        .patch_settings(&w::UpdateSettingsRequest {
            revision: before.settings.revision + 1,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api(e) if e.body.error.code == "revision_conflict"));
    // The retired repeated-list tag must fail rather than masquerade as omission.
    use buffa::Message;
    let mut legacy = w::UpdateSettingsRequest {
        revision: before.settings.revision,
        ..Default::default()
    }
    .encode_to_vec();
    legacy.extend_from_slice(&[0x12, 0x00]);
    let error = host
        .client
        .patch_settings(&w::UpdateSettingsRequest::decode_from_slice(&legacy).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Api(e) if e.body.error.code == "invalid_request"));
    assert_eq!(
        host.client.snapshot(None).await.unwrap().cursor,
        before.cursor
    );
}

#[tokio::test]
async fn companion_documents_are_atomic_notified_durable_and_explicitly_clearable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.db");
    let store = Store::open(path.clone()).await.unwrap();
    let mut config = store.configuration().await.unwrap();
    let tts = uuid::Uuid::new_v4().to_string();
    config.settings.connections = vec![w::Connection {
        id: tts.clone(),
        kind: "tts".into(),
        adapter: "fixture".into(),
        endpoint: "https://tts.test".into(),
        options: document(json!({})).into(),
        ..Default::default()
    }];
    store
        .commit_configuration(ConfigurationCommit::Update(config))
        .await
        .unwrap();
    let host = Host::serve(
        crate::bootstrap::application(store.clone(), "private-token".into()),
        temp,
    )
    .await;
    let mut events = host.client.subscribe().await.unwrap();
    let before = host.client.snapshot(None).await.unwrap();
    let original = before.companions[0].clone();
    let stored_original = store.snapshot().await.unwrap().companions[0].clone();
    let body =
        json!({"format":"vrm1", "environment_id":before.environment_id, "model":"avatar.vrm"});
    let voice = json!({"connection_id":tts, "voice_id":"voice1"});
    let valid = w::UpdateCompanionRequest {
        revision: original.revision,
        name: Some("configured".into()),
        body: set_document(body.clone()).into(),
        voice: set_document(voice.clone()).into(),
        ..Default::default()
    };
    let mut bad_voice = valid.clone();
    bad_voice.voice = set_document(json!({"connection_id":"missing", "voice_id":"voice1"})).into();
    let mut bad_body = valid.clone();
    bad_body.body =
        set_document(json!({"format":"vrm1", "environment_id":"other", "model":"avatar.vrm"}))
            .into();
    let mut empty_patch = valid.clone();
    empty_patch.voice = w::JsonDocumentPatch::default().into();
    let mut bad_assignment = valid.clone();
    bad_assignment.companion_assignment = w::AssignmentPatch {
        change: Some(w::assignment_patch::Change::Set(Box::new(
            w::ModelAssignment {
                connection_id: "missing".into(),
                model_id: "model".into(),
                ..Default::default()
            },
        ))),
        ..Default::default()
    }
    .into();
    for invalid in [bad_voice, bad_body, empty_patch, bad_assignment] {
        assert!(
            host.client
                .patch_companion(&original.id, &invalid)
                .await
                .is_err()
        );
        assert_eq!(host.client.companions().await.unwrap()[0], original);
        assert_eq!(
            store.snapshot().await.unwrap().companions[0],
            stored_original
        );
        assert_eq!(
            host.client.snapshot(None).await.unwrap().cursor,
            before.cursor
        );
    }
    let saved = host
        .client
        .patch_companion(&original.id, &valid)
        .await
        .unwrap();
    assert_eq!(saved.revision, original.revision + 1);
    assert_eq!(saved.body, document(body).into());
    assert_eq!(saved.voice, document(voice).into());
    let Some(ene_api_client::ServerEvent::Notification(event)) =
        timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("companion event missing")
    };
    assert!(
        matches!(event.event, Some(w::notification::Event::CompanionUpdated(c)) if *c == saved)
    );
    let restarted = crate::bootstrap::application(
        Store::open(path.clone()).await.unwrap(),
        "other-token".into(),
    );
    assert_eq!(
        restarted
            .application
            .snapshot(None)
            .await
            .unwrap()
            .stored
            .companions[0],
        saved
    );
    let stale = host
        .client
        .patch_companion(&original.id, &valid)
        .await
        .unwrap_err();
    assert!(matches!(stale, Error::Api(e) if e.body.error.code == "revision_conflict"));
    // JSON null is omission, so a name-only edit cannot erase the documents.
    let rename: w::UpdateCompanionRequest = serde_json::from_value(
        json!({"revision":saved.revision, "name":"renamed", "body":null, "voice":null}),
    )
    .unwrap();
    let renamed = host
        .client
        .patch_companion(&saved.id, &rename)
        .await
        .unwrap();
    assert_eq!(renamed.body, saved.body);
    assert_eq!(renamed.voice, saved.voice);
    let settings = host.client.settings().await.unwrap();
    let remove = w::UpdateSettingsRequest {
        revision: settings.revision,
        connections: w::ConnectionInputs::default().into(),
        ..Default::default()
    };
    let error = host.client.patch_settings(&remove).await.unwrap_err();
    assert!(matches!(error, Error::Api(e) if e.body.error.code == "state_conflict"));
    let cleared = host
        .client
        .patch_companion(
            &saved.id,
            &w::UpdateCompanionRequest {
                revision: renamed.revision,
                body: clear_document().into(),
                voice: clear_document().into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!cleared.body.is_set() && !cleared.voice.is_set());
    assert!(
        host.client
            .patch_settings(&remove)
            .await
            .unwrap()
            .connections
            .is_empty()
    );
    let restarted =
        crate::bootstrap::application(Store::open(path).await.unwrap(), "other-token".into());
    let snapshot = restarted.application.snapshot(None).await.unwrap();
    assert!(
        !snapshot.stored.companions[0].body.is_set()
            && !snapshot.stored.companions[0].voice.is_set()
    );
    assert!(snapshot.stored.settings.connections.is_empty());
}

struct TestAudioEngine;
impl crate::audio::Engine for TestAudioEngine {
    fn run(
        &self,
        start: w::AudioStart,
        mut input: tokio::sync::mpsc::Receiver<w::AudioChunk>,
        output: tokio::sync::mpsc::Sender<w::AudioFrame>,
        mut cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ()>> + Send>> {
        Box::pin(async move {
            let mut sequence = 0;
            loop {
                tokio::select! {biased;
                    _=cancelled.changed()=>return Ok(()),
                    chunk=input.recv()=>match chunk {
                        Some(chunk)=>{output.send(w::AudioFrame{frame:Some(w::audio_frame::Frame::Chunk(Box::new(w::AudioChunk{session_id:start.session_id.clone(),sequence,data:chunk.data,..Default::default()}))),..Default::default()}).await.map_err(|_|())?;sequence+=1;},
                        None=>return Ok(()),
                    }
                }
            }
        })
    }
}
#[tokio::test]
async fn audio_is_duplex_and_stop_or_revocation_fences_old_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("host.db")).await.unwrap();
    let mut app = crate::bootstrap::application(store, "private-token".into());
    app.audio = crate::audio::Audio::with_engine(std::sync::Arc::new(TestAudioEngine));
    let host = Host::serve(app, temp).await;
    let (device, native) = host.device(&["audio.input", "audio.output"]).await;
    let companion = host.client.companions().await.unwrap().remove(0).id;
    let start = native.prepare_audio(&companion, &device.id).await.unwrap();
    let mut stream = native.start_audio(start.clone()).await.unwrap();
    stream.send_chunk(vec![1, 2, 3, 4]).await.unwrap();
    let Some(ene_api_client::AudioEvent::Chunk(chunk)) =
        timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("audio output missing");
    };
    assert_eq!(&*chunk.data, &[1, 2, 3, 4]);
    assert_eq!(chunk.session_id, start.session_id);
    let pending = native.prepare_audio(&companion, &device.id).await.unwrap();
    timeout(Duration::from_secs(1), host.client.stop(&companion))
        .await
        .unwrap()
        .unwrap();
    let Some(ene_api_client::AudioEvent::Failed(failure)) =
        timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("cancel missing");
    };
    assert_eq!(failure.code, "cancelled");
    assert!(stream.next().await.unwrap().is_none());
    assert!(native.start_audio(pending).await.is_err());
    let start = native.prepare_audio(&companion, &device.id).await.unwrap();
    assert_eq!(start.generation, 1);
    let pending_revoke = native.prepare_audio(&companion, &device.id).await.unwrap();
    let mut stream = native.start_audio(start).await.unwrap();
    host.client
        .device_rpc()
        .set_permission(w::DevicePermissionRequest {
            device_id: device.id.clone(),
            capability: "audio.input".into(),
            allowed: false,
            ..Default::default()
        })
        .await
        .unwrap();
    let Some(ene_api_client::AudioEvent::Failed(failure)) =
        timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("revoke missing");
    };
    assert_eq!(failure.code, "permission_required");
    host.client
        .device_rpc()
        .set_permission(w::DevicePermissionRequest {
            device_id: device.id.clone(),
            capability: "audio.input".into(),
            allowed: true,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(native.start_audio(pending_revoke).await.is_err());
    let start = native.prepare_audio(&companion, &device.id).await.unwrap();
    let (mut input, mut output) = native.start_audio(start).await.unwrap().into_split();
    // More than both bounded engine queues can hold: input and output must
    // progress simultaneously, and output must drain after microphone EOF.
    timeout(Duration::from_secs(3), async {
        let sending = async {
            for _ in 0..40 {
                input.send_chunk(vec![3, 4]).await.unwrap();
            }
            input.end_input().await.unwrap();
            drop(input);
        };
        let receiving = async {
            for sequence in 0..40 {
                let Some(ene_api_client::AudioEvent::Chunk(chunk)) = output.next().await.unwrap()
                else {
                    panic!("audio output missing");
                };
                assert_eq!(chunk.sequence, sequence);
                assert_eq!(&*chunk.data, &[3, 4]);
            }
            assert!(matches!(
                output.next().await.unwrap(),
                Some(ene_api_client::AudioEvent::Ended)
            ));
        };
        tokio::join!(sending, receiving);
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn device_queue_and_display_leases_are_bounded_and_revocation_is_final() {
    let host = Host::new().await;
    let (device, _) = host.device(&["tools.test", "companion.display"]).await;
    let (other, _) = host.device(&["companion.display"]).await;
    // Direct subscription deliberately never drains commands, exercising a slow receiver.
    let subscription = host.app.devices.subscribe(device.id.clone()).await.unwrap();
    for index in 0..32 {
        host.app
            .devices
            .request(w::DeviceOperationRequest {
                device_id: device.id.clone(),
                operation_id: format!("op-{index}"),
                capability: "tools.test".into(),
                input: w::JsonDocument {
                    json: "{}".into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .unwrap();
    }
    let full = host
        .app
        .devices
        .request(w::DeviceOperationRequest {
            device_id: device.id.clone(),
            operation_id: "overflow".into(),
            capability: "tools.test".into(),
            input: w::JsonDocument {
                json: "{}".into(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        })
        .await
        .err()
        .unwrap();
    assert_eq!(full.code, connectrpc::ErrorCode::ResourceExhausted);
    host.app
        .devices
        .permission(device.id.clone(), Some("tools.test".into()), false)
        .await
        .unwrap();
    assert_eq!(
        host.app
            .devices
            .operation(&device.id, "op-0")
            .await
            .unwrap()
            .status,
        w::OperationStatus::Revoked
    );
    drop(subscription);
    let companion = host.client.companions().await.unwrap().remove(0).id;
    let first = host
        .app
        .devices
        .activate(w::ActivationRequest {
            companion_id: companion.clone(),
            device_id: device.id.clone(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        host.app
            .devices
            .activate(w::ActivationRequest {
                companion_id: companion.clone(),
                device_id: other.id.clone(),
                ..Default::default()
            })
            .await
            .is_err()
    );
    host.app
        .devices
        .activate(w::ActivationRequest {
            companion_id: companion.clone(),
            device_id: device.id,
            expected_revision: Some(first.revision),
            release: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let next = host
        .app
        .devices
        .activate(w::ActivationRequest {
            companion_id: companion,
            device_id: other.id,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(next.revision > first.revision);
    assert_eq!(next.lease_ms, 15_000);
    let restarted = crate::devices::Devices::new(Some(
        Store::open(host.temp.path().join("host.db")).await.unwrap(),
    ));
    assert!(
        restarted
            .activate(w::ActivationRequest {
                companion_id: next.companion_id,
                device_id: next.device_id,
                ..Default::default()
            })
            .await
            .is_err()
    );
}
#[tokio::test]
async fn speech_is_not_advertised_without_an_engine() {
    let host = Host::new().await;
    let (device, native) = host.device(&["audio.input", "audio.output"]).await;
    let snapshot = host.client.snapshot(None).await.unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&snapshot.capabilities.json).unwrap()["transport"]
            ["audio_available"],
        false
    );
    assert!(
        native
            .prepare_audio(&snapshot.companions[0].id, &device.id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn approved_native_devices_can_sync_and_stop_but_revocation_closes_their_host_stream() {
    let host = Host::new().await;
    let (device, native) = host.device(&[]).await;
    assert!(matches!(native.settings().await, Err(Error::Api(error)) if error.status==403));
    let mut stream = native.subscribe().await.unwrap();
    let snapshot = stream.take_initial_snapshot().unwrap();
    let companion = &snapshot.companions[0];
    assert_eq!(native.companions().await.unwrap()[0].id, companion.id);
    native.stop(&companion.id).await.unwrap();
    host.client
        .device_rpc()
        .revoke(w::Id {
            id: device.id,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(2), stream.next())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        native.snapshot(None).await,
        Err(Error::Unauthorized)
    ));
}
