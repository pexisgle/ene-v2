//! Host snapshot, subscription and companion/mind services.
use super::Services;
use crate::host::{self, Subscription, mind};
use connectrpc::{
    CodecFormat, ConnectError, Encodable, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};
use ene_rpc as w;
use futures_util::StreamExt;
use std::sync::Arc;

#[expect(refining_impl_trait, reason = "Connect boxes futures")]
impl w::HostService for Services {
    async fn get_snapshot(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Snapshot> {
        self.inspect(|services| host::SnapshotReader::from(services).get())
            .await
    }

    #[tracing::instrument(
        name = "rpc",
        skip_all,
        fields(
            method = "subscribe",
            companion_id = tracing::field::Empty,
            request_id = tracing::field::Empty,
            client_id = %request.client_id,
        )
    )]
    async fn subscribe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::SubscribeRequest>,
    ) -> ServiceResult<ServiceStream<NotificationBody>> {
        let subscriber = (!request.client_id.is_empty()).then(|| request.client_id.to_owned());
        let diagnostics = request.include_diagnostics;
        let subscriber_view = subscriber.clone();
        let cursor = request.cursor.as_option().map(|cursor| w::Cursor {
            epoch: cursor.epoch.to_owned(),
            seq: cursor.seq,
            ..Default::default()
        });
        // A cursor from this epoch resumes from the replay ring; an unknown or
        // stale cursor (or a gap) falls back to a fresh snapshot. The replay
        // and the receiver come from one host operation, so a tick cannot slip
        // an event between them.
        let Subscription {
            cursor: target_cursor,
            backlog,
            mut live,
        } = self
            .runtime
            .inspect(move |services| {
                host::SnapshotReader::from(services)
                    .subscribe(subscriber.as_deref(), cursor.as_ref())
            })
            .await
            .map_err(ConnectError::from)?;
        let stream = async_stream::stream! {
            for event in backlog {
                if visible(&event, subscriber_view.as_deref(), diagnostics) {
                    yield Ok(NotificationBody(event));
                }
            }
            loop {
                match live.recv().await {
                    Ok(event) => {
                        if visible(&event, subscriber_view.as_deref(), diagnostics) {
                            yield Ok(NotificationBody(event));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        yield Ok(NotificationBody(Arc::new(w::Notification {
                            kind: "resync".into(),
                            event: Some(w::notification::Event::ResyncRequired(Box::new(
                                w::ResyncRequired {
                                    reason: "subscription lagged".into(),
                                    target_cursor: Some(target_cursor).into(),
                                    ..Default::default()
                                },
                            ))),
                            ..Default::default()
                        })));
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        .take_until(self.shutdown.clone().cancelled_owned());
        Response::stream_ok(stream)
    }

    async fn get_perception_frame(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::ScreenFrame> {
        tracing::Span::current().record("request_id", request.id);
        let id = request.id.to_owned();
        self.read(move |kernel| kernel.screen_preview(&id)).await
    }

    async fn list_companions(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::Companions> {
        self.with_store(|store| {
            Ok(w::Companions {
                items: mind::list(&store)?,
                ..Default::default()
            })
        })
        .await
    }

    async fn get_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, ene_rpc::Id>,
    ) -> connectrpc::ServiceResult<w::Companion> {
        let id = request.id.to_owned();
        // `mind::get` takes only the store, so this never needs the actor thread.
        self.with_store(move |store| mind::get(&store, &id)).await
    }
    async fn create_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::CreateCompanionRequest>,
    ) -> connectrpc::ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.create_companion(request))
            .await
    }
    async fn update_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::UpdateCompanionRequest>,
    ) -> connectrpc::ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.update_companion(request))
            .await
    }
    async fn delete_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::Id>,
    ) -> connectrpc::ServiceResult<ene_rpc::Empty> {
        let id = request.id.to_owned();
        self.call(move |kernel| {
            kernel.delete_companion(&id)?;
            Ok(ene_rpc::Empty::default())
        })
        .await
    }

    async fn list_companion_templates(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::Empty>,
    ) -> ServiceResult<w::CompanionTemplates> {
        Response::ok(mind::list_templates())
    }

    async fn export_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, ene_rpc::Id>,
    ) -> connectrpc::ServiceResult<w::CompanionTemplate> {
        let id = request.id.to_owned();
        // `mind::export_template` takes only the store, so this never needs the actor thread.
        self.with_store(move |store| mind::export_template(&store, &id))
            .await
    }
    async fn import_companion(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::ImportCompanionRequest>,
    ) -> connectrpc::ServiceResult<w::Companion> {
        let request = request.to_owned_message();
        self.call(move |kernel| kernel.import_companion(request))
            .await
    }

    async fn preview_companion(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::PreviewCompanionRequest>,
    ) -> ServiceResult<w::PreviewCompanionResponse> {
        let req = request.to_owned_message();
        let system_prompt = mind::preview_system_prompt(&req).map_err(ConnectError::from)?;
        let requested_model = req
            .model
            .into_option()
            .map_or_default(|m| mind::ModelAssignment {
                connection_id: m.connection_id,
                model_id: m.model_id,
            });
        let user_message = req.user_message;
        let model = self
            .runtime
            .read_kernel(move |kernel| kernel.preview_model(requested_model))
            .await
            .map_err(ConnectError::from)?;
        let reply = host::model::preview_reply(model, system_prompt, user_message)
            .await
            .map_err(ConnectError::from)?;
        Response::ok(w::PreviewCompanionResponse {
            reply,
            ..Default::default()
        })
    }

    async fn get_mind(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, ene_rpc::Id>,
    ) -> connectrpc::ServiceResult<w::MindView> {
        let id = request.id.to_owned();
        // `mind::mind_view` takes only the store, so this never needs the actor thread.
        self.with_store(move |store| mind::mind_view(&store, &id))
            .await
    }
    async fn update_mind_profile(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::UpdateMindProfileRequest>,
    ) -> connectrpc::ServiceResult<w::MindView> {
        let request = request.to_owned_message();
        self.change(move |services| {
            mind::update_profile(&services.store, &services.notifier, &request)
        })
        .await
    }
    async fn control_memory(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::MemoryControlRequest>,
    ) -> connectrpc::ServiceResult<w::MemoryControlResult> {
        let request = request.to_owned_message();
        self.change(move |services| {
            mind::control_memory(&services.store, &services.notifier, &request)
        })
        .await
    }
    async fn list_memories(
        &self,
        _: connectrpc::RequestContext,
        request: connectrpc::ServiceRequest<'_, w::ListMemoriesRequest>,
    ) -> connectrpc::ServiceResult<w::Memories> {
        let request = request.to_owned_message();
        // `mind::list_memories` takes only the store, so this never needs the actor thread.
        self.with_store(move |store| mind::list_memories(&store, &request))
            .await
    }
}

pub(crate) struct NotificationBody(Arc<w::Notification>);

impl Encodable<w::Notification> for NotificationBody {
    fn encode(&self, codec: CodecFormat) -> Result<bytes::Bytes, ConnectError> {
        self.0.as_ref().encode(codec)
    }
}

/// A notification addressed to one client is not sent to another.
fn visible(event: &w::Notification, subscriber: Option<&str>, diagnostics: bool) -> bool {
    if event.kind == host::notify::DIAGNOSTIC {
        return diagnostics;
    }
    match (event.client_id.as_deref(), subscriber) {
        (Some(target), Some(subscriber)) => target == subscriber,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::db::Store;
    use crate::host::{Runtime, db::clients::ClientDeclaration, notify::Notifier};
    use connectrpc::StreamMessage;
    use tokio_util::sync::CancellationToken;
    use w::HostService;

    fn event(label: &str) -> w::notification::Event {
        w::notification::Event::Generic(Box::new(w::HostEvent {
            kind: label.into(),
            ..Default::default()
        }))
    }

    #[tokio::test]
    async fn subscription_replays_and_filters_after_request_body_is_dropped() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        // The boot's reconciliation queues `host_ready` after the first frame,
        // so one more frame delivers it; a cursor taken before that replays it.
        runtime.call(|_| Ok(())).await.unwrap();
        let cursor = runtime
            .read(|app| {
                app.world()
                    .resource::<Store>()
                    .client_repo()
                    .register_client(&ClientDeclaration {
                        id: "c1",
                        name: "desk",
                        platform: "linux",
                        capabilities: &[],
                        specs: &[],
                        expected_revision: 0,
                    })?;
                Ok(app.world().resource::<Notifier>().cursor())
            })
            .await
            .unwrap();
        runtime
            .call(|app| {
                let notifier = app.world_mut().resource_mut::<Notifier>();
                notifier.push(None, Some("other"), event("hidden"));
                notifier.diagnostic(
                    "ene",
                    "eyes",
                    "screen_received",
                    &serde_json::json!({"bytes": 10}),
                );
                notifier.push(None, Some("c1"), event("replayed"));
                Ok(())
            })
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: shutdown.clone(),
        };
        let mut stream = {
            let message = StreamMessage::from_message(&w::SubscribeRequest {
                client_id: "c1".into(),
                cursor: Some(cursor.clone()).into(),
                ..Default::default()
            });
            service
                .subscribe(
                    RequestContext::new(http::HeaderMap::new()),
                    ServiceRequest::from_parts(message.view(), message.bytes()),
                )
                .await
                .unwrap()
                .body
        };
        let NotificationBody(replayed) = stream.next().await.unwrap().unwrap();
        assert_eq!(replayed.client_id.as_deref(), Some("c1"));
        assert!(
            matches!(replayed.event, Some(w::notification::Event::Generic(ref item)) if item.kind == "replayed")
        );
        {
            let message = StreamMessage::from_message(&w::SubscribeRequest {
                client_id: "c1".into(),
                cursor: Some(cursor).into(),
                include_diagnostics: true,
                ..Default::default()
            });
            let mut diagnostics = service
                .subscribe(
                    RequestContext::new(http::HeaderMap::new()),
                    ServiceRequest::from_parts(message.view(), message.bytes()),
                )
                .await
                .unwrap()
                .body;
            let NotificationBody(diagnostic) = diagnostics.next().await.unwrap().unwrap();
            assert_eq!(diagnostic.kind, host::notify::DIAGNOSTIC);
        }
        runtime
            .call(|app| {
                let notifier = app.world_mut().resource_mut::<Notifier>();
                notifier.push(None, Some("other"), event("hidden live"));
                notifier.push(None, Some("c1"), event("live"));
                Ok(())
            })
            .await
            .unwrap();
        let NotificationBody(live) = stream.next().await.unwrap().unwrap();
        assert!(
            matches!(live.event, Some(w::notification::Event::Generic(ref item)) if item.kind == "live")
        );
        assert!(live.cursor.as_option().unwrap().seq > replayed.cursor.as_option().unwrap().seq);
        shutdown.cancel();
        assert!(stream.next().await.is_none());
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn f03_export_and_import_template_strips_host_local_connection_ids() {
        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .call(|app| {
                let store = app.world().resource::<Store>().clone();
                let mut ene = store.companion_repo().companion("ene")?.unwrap();
                ene.voice = ene_mind::JsonDoc::new(
                    "ene.voice.v1",
                    serde_json::json!({
                        "connection_id": "local-secret-conn-1",
                        "voice_id": "3",
                        "speed": 1.1
                    }),
                );
                store.companion_repo().update_companion(&ene, 1)?;
                Ok(())
            })
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: shutdown.clone(),
        };

        let export_msg = StreamMessage::from_message(&w::Id {
            id: "ene".into(),
            ..Default::default()
        });
        let template = service
            .export_companion(
                RequestContext::new(http::HeaderMap::new()),
                ServiceRequest::from_parts(export_msg.view(), export_msg.bytes()),
            )
            .await
            .unwrap()
            .body;
        assert_eq!(template.format_version, 1);
        let voice_doc: ene_domain::VoiceBinding =
            serde_json::from_str(&template.voice.as_option().unwrap().json).unwrap();
        assert_eq!(voice_doc.connection_id, None);
        assert_eq!(voice_doc.voice_id.as_deref(), Some("3"));
        assert_eq!(voice_doc.speed, Some(1.1));

        let mut imported_template = template;
        imported_template.name = "エネ・クローン".into();
        let import_msg = StreamMessage::from_message(&w::ImportCompanionRequest {
            template: Some(imported_template).into(),
            assignments: Some(w::ModelAssignments {
                companion: Some(w::ModelAssignment {
                    connection_id: "c1".into(),
                    model_id: "gpt-4o-mini".into(),
                    ..Default::default()
                })
                .into(),
                task: Some(w::ModelAssignment {
                    connection_id: "c1".into(),
                    model_id: "gpt-4o-mini".into(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        let created = service
            .import_companion(
                RequestContext::new(http::HeaderMap::new()),
                ServiceRequest::from_parts(import_msg.view(), import_msg.bytes()),
            )
            .await
            .unwrap()
            .body;
        assert_eq!(created.name, "エネ・クローン");
        assert_ne!(created.id, "ene");
        runtime.shutdown().await.unwrap();
    }

    async fn spawn_preview_response_server() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            loop {
                let n = stream.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&buf[..pos]);
                    let cl = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + cl {
                        break;
                    }
                }
            }
            let body = serde_json::json!({
                "id": "resp_preview",
                "object": "response",
                "created_at": 1,
                "status": "completed",
                "model": "gpt-4o-mini",
                "output": [{
                    "type": "message",
                    "id": "msg_preview",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": "はじめまして、プレビュー応答です！",
                        "annotations": []
                    }]
                }],
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 5,
                    "total_tokens": 10
                }
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            ene_domain::debug_if_err(
                stream.write_all(reply.as_bytes()).await,
                "event reply write",
            );
            ene_domain::debug_if_err(stream.shutdown().await, "event stream shutdown");
        });
        addr
    }

    #[tokio::test]
    async fn f05_preview_companion_replies_without_persisting_companion_or_messages() {
        let addr = spawn_preview_response_server().await;

        let state = tempfile::tempdir().unwrap();
        let runtime = Runtime::start(state.path()).await.unwrap();
        runtime
            .call(move |app| {
                let store = app.world().resource::<Store>().clone();
                let conn = host::settings::Connection {
                    id: ene_domain::ConnectionId::parse("llm-preview").unwrap(),
                    name: "Preview LLM".into(),
                    enabled: true,
                    kind: host::settings::ConnectionKind::Llm,
                    config: host::settings::ConnectionConfig::Llm(host::settings::LlmConnection {
                        endpoint: host::settings::Endpoint {
                            adapter: "openai".into(),
                            endpoint: format!("http://{addr}/v1"),
                            options_schema: String::new(),
                            options: "{}".into(),
                        },
                        model_ids: vec!["gpt-4o-mini".into()],
                    }),
                    credential_state: host::settings::CredentialState::Absent,
                    revision: 1,
                    last_check: None,
                };
                store.settings_repo().update_settings(1, &[(conn, 0)])?;
                app.world()
                    .resource::<host::model::Credentials>()
                    .remember("llm-preview", "test-key".into());
                Ok(())
            })
            .await
            .unwrap();
        let shutdown = CancellationToken::new();
        let service = Services {
            runtime: runtime.clone(),
            shutdown: shutdown.clone(),
        };
        let req = StreamMessage::from_message(&w::PreviewCompanionRequest {
            name: "テスト相棒".into(),
            mind: "短く元気にあいさつするテスト用の相棒。".into(),
            user_message: "よろしく！".into(),
            model: Some(w::ModelAssignment {
                connection_id: "llm-preview".into(),
                model_id: "gpt-4o-mini".into(),
                ..Default::default()
            })
            .into(),
            ..Default::default()
        });
        let resp = service
            .preview_companion(
                RequestContext::new(http::HeaderMap::new()),
                ServiceRequest::from_parts(req.view(), req.bytes()),
            )
            .await
            .unwrap()
            .body;
        assert_eq!(resp.reply, "はじめまして、プレビュー応答です！");
        let count = runtime
            .read(|app| {
                Ok(app
                    .world()
                    .resource::<Store>()
                    .companion_repo()
                    .companions()?
                    .len())
            })
            .await
            .unwrap();
        assert_eq!(count, 1, "preview must not create a companion row");
        runtime.shutdown().await.unwrap();
    }
}
