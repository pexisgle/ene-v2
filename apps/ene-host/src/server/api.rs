use super::auth;
use crate::{actor::Host, error::rpc, mcp::Mcp, store};
use anyhow::Result;
use axum::middleware;
use connectrpc::{
    ConnectError, InboundStream, RequestContext, Response, ServiceRequest, ServiceResult,
    ServiceStream,
};
use ene_rpc::{self as w, event::Payload};
use futures_util::StreamExt as _;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

pub(super) fn router(host: Host, guard: auth::Guard) -> axum::Router {
    let api = Arc::new(Api(host));
    let router = w::HostServiceExt::register(api.clone(), connectrpc::Router::new());
    let router = w::ChatServiceExt::register(api.clone(), router);
    let router = w::SettingsServiceExt::register(api.clone(), router);
    let router = w::WorkServiceExt::register(api.clone(), router);
    let router = w::VoiceServiceExt::register(api.clone(), router);
    let router = w::ClientServiceExt::register(api, router);
    let router = router.with_route_limits(
        "/ene.host.v2.HostService/ImportCharacter",
        connectrpc::Limits::default().with_max_message_size(48 * 1024 * 1024),
    );
    axum::Router::new()
        .nest_service("/rpc", router.into_axum_router())
        .layer(middleware::from_fn_with_state(guard, auth::authenticate))
}

struct Api(Host);

macro_rules! unary {
    ($method:ident, $input:ty, $output:ty, |$actor:ident, $request:ident| $body:expr) => {
        async fn $method(
            &self,
            _: RequestContext,
            request: ServiceRequest<'_, $input>,
        ) -> ServiceResult<$output> {
            let $request = request.to_owned_message();
            let value = self
                .0
                .call(move |$actor| Box::pin(async move { $body }))
                .await
                .map_err(rpc)?;
            Response::ok(value)
        }
    };
}

#[allow(refining_impl_trait)]
impl w::HostService for Api {
    unary!(get_snapshot, w::Empty, w::Snapshot, |actor, _req| actor
        .snapshot()
        .await);
    unary!(
        put_character,
        w::CharacterChange,
        w::Character,
        |actor, req| actor.put_character(req).await
    );
    unary!(
        delete_character,
        w::DeleteRequest,
        w::Empty,
        |actor, req| actor.delete_record("character", req).await
    );
    unary!(
        put_companion,
        w::CompanionChange,
        w::Companion,
        |actor, req| actor.put_companion(req).await
    );
    unary!(
        delete_companion,
        w::DeleteRequest,
        w::Empty,
        |actor, req| actor.delete_record("companion", req).await
    );
    unary!(
        export_character,
        w::ExportRequest,
        w::CharacterPackage,
        |actor, req| actor.export_character(req).await
    );
    unary!(
        import_character,
        w::ImportRequest,
        w::Character,
        |actor, req| actor.import_character(req).await
    );
    unary!(get_mind, w::Id, w::Mind, |actor, req| actor
        .mind(&req.id)
        .await);
    unary!(put_mind, w::MindChange, w::Mind, |actor, req| actor
        .put_mind(req)
        .await);
    unary!(
        search_memories,
        w::MemoryQuery,
        w::Memories,
        |actor, req| actor.search_memories(req).await
    );
    unary!(change_memory, w::MemoryChange, w::Memory, |actor, req| {
        actor.change_memory(req).await
    });

    async fn preview(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::PreviewRequest>,
    ) -> ServiceResult<w::Text> {
        let request = request.to_owned_message();
        let profile = request
            .profile
            .into_option()
            .ok_or_else(|| ConnectError::invalid_argument("profile required"))?;
        crate::configuration::validate_profile(&profile).map_err(rpc)?;
        if request.input.trim().is_empty() || request.input.len() > 4096 {
            return Err(ConnectError::invalid_argument("invalid preview input"));
        }
        let selection = request
            .model
            .into_option()
            .ok_or_else(|| ConnectError::invalid_argument("model required"))?;
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move {
                    let connection = actor.connection(&selection.connection_id).await?;
                    Ok((actor.models.clone(), (selection, connection)))
                })
            })
            .await
            .map_err(rpc)?;
        let model = models
            .model(connection.0, connection.1)
            .await
            .map_err(rpc)?;
        let system = format!(
            "Reply once using the following fictional character profile.\n{}",
            serde_json::to_string(&profile).map_err(|error| rpc(error.into()))?
        );
        let text = model.text(&system, &request.input).await.map_err(rpc)?;
        Response::ok(w::Text {
            text,
            ..Default::default()
        })
    }

    async fn subscribe(
        &self,
        _: RequestContext,
        _: ServiceRequest<'_, w::SubscribeRequest>,
    ) -> ServiceResult<ServiceStream<w::Event>> {
        let host = self.0.clone();
        let (snapshot, mut events) = host
            .call(|actor| {
                Box::pin(async { Ok((actor.snapshot().await?, actor.events.subscribe())) })
            })
            .await
            .map_err(rpc)?;
        let stream: ServiceStream<w::Event> = Box::pin(async_stream::try_stream! {
            yield snapshot_event(snapshot);
            loop {
                let event = tokio::select! {
                    () = host.shutdown.cancelled() => break,
                    event = events.recv() => event,
                };
                match event {
                    Ok(event) => yield event,
                    Err(RecvError::Lagged(_)) => {
                        let (snapshot, fresh) = host.call(|actor| Box::pin(async {
                            Ok((actor.snapshot().await?, actor.events.subscribe()))
                        })).await.map_err(rpc)?;
                        events = fresh;
                        yield snapshot_event(snapshot);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
        Response::ok(stream)
    }
}

fn snapshot_event(snapshot: w::Snapshot) -> w::Event {
    w::Event {
        cursor: snapshot.cursor.clone(),
        payload: Some(Payload::Snapshot(Box::new(snapshot))),
        ..Default::default()
    }
}

#[allow(refining_impl_trait)]
impl w::ChatService for Api {
    unary!(send, w::SendRequest, w::Receipt, |actor, req| actor
        .send(req)
        .await);
    unary!(stop, w::StopRequest, w::Empty, |actor, req| actor
        .stop_chat(req)
        .await);
    unary!(get_history, w::HistoryRequest, w::History, |actor, req| {
        actor.history(req).await
    });
}

#[allow(refining_impl_trait)]
impl w::SettingsService for Api {
    unary!(get, w::Empty, w::Settings, |actor, _req| actor
        .settings()
        .await);
    unary!(
        put_connection,
        w::ConnectionChange,
        w::Connection,
        |actor, req| actor.put_connection(req).await
    );
    unary!(
        delete_connection,
        w::DeleteRequest,
        w::Empty,
        |actor, req| actor.delete_record("connection", req).await
    );
    unary!(
        set_credential,
        w::CredentialChange,
        w::Empty,
        |actor, req| actor.set_credential(req).await
    );
    unary!(put_policy, w::PolicyChange, w::Policy, |actor, req| actor
        .put_policy(req)
        .await);
    unary!(delete_rule, w::DeleteRequest, w::Empty, |actor, req| actor
        .delete_record("rule", req)
        .await);
    unary!(answer, w::ApprovalAnswer, w::Empty, |actor, req| actor
        .answer(req)
        .await);
    unary!(put_plugin, w::PluginChange, w::Plugin, |actor, req| actor
        .put_plugin(req)
        .await);
    unary!(delete_plugin, w::DeleteRequest, w::Empty, |actor, req| {
        actor.delete_record("plugin", req).await
    });
    unary!(audit_history, w::AuditQuery, w::Audit, |actor, req| {
        Ok(w::Audit {
            items: store::audit_history(&actor.db, req.before, req.limit).await?,
            ..Default::default()
        })
    });

    async fn models(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::Id>,
    ) -> ServiceResult<w::ModelList> {
        let id = request.to_owned_message().id;
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move { Ok((actor.models.clone(), actor.connection(&id).await?)) })
            })
            .await
            .map_err(rpc)?;
        let model = models
            .model(w::ModelSelection::default(), connection)
            .await
            .map_err(rpc)?;
        let ids = model.models().await.map_err(rpc)?;
        Response::ok(w::ModelList {
            ids,
            ..Default::default()
        })
    }

    async fn probe(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::ProbeRequest>,
    ) -> ServiceResult<w::Empty> {
        let request = request.to_owned_message();
        let (models, connection) = self
            .0
            .call(move |actor| {
                Box::pin(async move {
                    Ok((
                        actor.models.clone(),
                        actor.connection(&request.connection_id).await?,
                    ))
                })
            })
            .await
            .map_err(rpc)?;
        if matches!(
            connection.kind.as_known(),
            Some(w::ProviderKind::McpHttp | w::ProviderKind::McpStdio)
        ) {
            let cancel = self.0.shutdown.child_token();
            let _guard = cancel.clone().drop_guard();
            let mcp = Mcp::open(connection, &models, &cancel).await.map_err(rpc)?;
            mcp.service
                .list_all_tools()
                .await
                .map_err(|_| ConnectError::unavailable("configured MCP unavailable"))?;
        } else if let Some(selection) = request.model.into_option() {
            let model = models.model(selection, connection).await.map_err(rpc)?;
            model.text("Reply briefly.", "OK").await.map_err(rpc)?;
        } else {
            let model = models
                .model(w::ModelSelection::default(), connection)
                .await
                .map_err(rpc)?;
            model.models().await.map_err(rpc)?;
        }
        Response::ok(w::Empty::default())
    }
}

#[allow(refining_impl_trait)]
impl w::WorkService for Api {
    unary!(list, w::Empty, w::WorkList, |actor, _req| {
        let mut db = actor.db.acquire().await?;
        Ok(w::WorkList {
            items: store::list(&mut db, "work").await?,
            ..Default::default()
        })
    });
    unary!(send, w::WorkInput, w::Receipt, |actor, req| actor
        .send_work(req)
        .await);
    unary!(stop, w::StopRequest, w::Empty, |actor, req| actor
        .stop_work(req)
        .await);
}

#[allow(refining_impl_trait)]
impl w::ClientService for Api {
    unary!(register_device, w::DeviceChange, w::Device, |actor, req| {
        actor.register_device(req).await
    });
    unary!(
        activate,
        w::ActivationRequest,
        w::Activation,
        |actor, req| actor.activate(req).await
    );
    unary!(report_perception, w::Perception, w::Empty, |actor, req| {
        actor.perception(req).await
    });
    unary!(claim, w::DeviceClaim, w::DeviceOperation, |actor, req| {
        actor.claim_device(req)
    });
    unary!(complete, w::DeviceResult, w::Empty, |actor, req| actor
        .complete_device(req));

    async fn operations(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::AudioSubscription>,
    ) -> ServiceResult<ServiceStream<w::DeviceOperation>> {
        let request = request.to_owned_message();
        let generation = request.generation;
        let host = self.0.clone();
        let (pending, mut rx) = host
            .call(move |actor| {
                Box::pin(async move {
                    actor.active(&request.device_id, generation)?;
                    let pending = actor
                        .device_ops
                        .values()
                        .filter(|op| op.device == request.device_id)
                        .map(|op| op.operation.clone())
                        .collect::<Vec<_>>();
                    Ok((pending, actor.operations.subscribe()))
                })
            })
            .await
            .map_err(rpc)?;
        let stream: ServiceStream<w::DeviceOperation> = Box::pin(async_stream::try_stream! {
            for value in pending { yield value; }
            while let Some(value) = receive(&host, &mut rx).await? {
                if value.generation == generation { yield value; } else { break; }
            }
        });
        Response::ok(stream)
    }
}

#[allow(refining_impl_trait)]
impl w::VoiceService for Api {
    unary!(speak, w::SpeakRequest, w::Empty, |actor, req| actor
        .speak(req)
        .await);

    async fn input(
        &self,
        _: RequestContext,
        mut requests: InboundStream<w::AudioInput>,
    ) -> ServiceResult<ServiceStream<w::Transcription>> {
        let host = self.0.clone();
        let stream: ServiceStream<w::Transcription> = Box::pin(async_stream::try_stream! {
            let mut buffer = Vec::new();
            let mut binding = None::<(String, u64, u32)>;
            loop {
                let item = tokio::select! {
                    () = host.shutdown.cancelled() => break,
                    item = requests.next() => item,
                };
                let Some(item) = item else { break; };
                let chunk = item?.to_owned_message();
                if chunk.interrupt {
                    let device = chunk.device_id.clone();
                    let generation = chunk.generation;
                    host.call(move |actor| Box::pin(async move {
                        actor.active(&device, generation)?;
                        actor.stop_speech();
                        Ok(())
                    })).await.map_err(rpc)?;
                }
                if !chunk.pcm.is_empty() {
                    let current = (chunk.device_id.clone(), chunk.generation, chunk.sample_rate);
                    if binding.as_ref().is_some_and(|previous| previous != &current) {
                        Err(ConnectError::aborted("audio source changed"))?;
                    }
                    binding = Some(current);
                    if buffer.len() + chunk.pcm.len() > 4 * 1024 * 1024 {
                        Err(ConnectError::resource_exhausted("audio input too large"))?;
                    }
                    buffer.extend(&chunk.pcm);
                }
                if chunk.commit && let Some((device, generation, rate)) = binding.take() {
                    if device != chunk.device_id || generation != chunk.generation || chunk.request_key.is_empty() {
                        Err(ConnectError::aborted("audio commit does not match its source"))?;
                    }
                    yield host.transcribe(device, generation, rate, std::mem::take(&mut buffer), chunk.request_key).await.map_err(rpc)?;
                }
            }
        });
        Response::ok(stream)
    }

    async fn output(
        &self,
        _: RequestContext,
        request: ServiceRequest<'_, w::AudioSubscription>,
    ) -> ServiceResult<ServiceStream<w::AudioOutput>> {
        let request = request.to_owned_message();
        let generation = request.generation;
        let host = self.0.clone();
        let mut rx = host
            .call(move |actor| {
                Box::pin(async move {
                    actor.active(&request.device_id, generation)?;
                    Ok(actor.audio.subscribe())
                })
            })
            .await
            .map_err(rpc)?;
        let stream: ServiceStream<w::AudioOutput> = Box::pin(async_stream::try_stream! {
            while let Some(value) = receive(&host, &mut rx).await? {
                if value.generation != generation { break; }
                let stop = value.stop;
                yield value;
                if stop {
                    let active = host.call(move |actor| Box::pin(async move { Ok(actor.activation.generation == generation) })).await.map_err(rpc)?;
                    if !active { break; }
                }
            }
        });
        Response::ok(stream)
    }
}

async fn receive<T: Clone>(
    host: &Host,
    receiver: &mut tokio::sync::broadcast::Receiver<T>,
) -> Result<Option<T>, ConnectError> {
    tokio::select! {
        () = host.shutdown.cancelled() => Ok(None),
        value = receiver.recv() => match value {
            Ok(value) => Ok(Some(value)),
            Err(RecvError::Closed) => Ok(None),
            Err(RecvError::Lagged(_)) => Err(ConnectError::unavailable("stream fell behind; reconnect")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::Actor;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn authenticated_h2c_clients_receive_an_initial_snapshot() -> Result<()> {
        let state = tempfile::tempdir()?;
        std::fs::create_dir(state.path().join("conversations"))?;
        let (host, actor) = Actor::start(state.path().to_owned()).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/rpc", listener.local_addr()?);
        let shutdown = CancellationToken::new();
        let server = tokio::spawn({
            let shutdown = shutdown.clone();
            let host = host.clone();
            async move {
                axum::serve(
                    listener,
                    router(
                        host,
                        auth::Guard::with_allowed_hosts("test-token".into(), ""),
                    ),
                )
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
            }
        });
        let (http, config) = w::connect(&endpoint, "test-token")?;
        let client = w::HostServiceClient::new(http, config);
        let mut stream = client.subscribe(w::SubscribeRequest::default()).await?;
        let Some(first) = stream.message::<w::Event>().await? else {
            anyhow::bail!("missing first snapshot");
        };
        let Some(Payload::Snapshot(snapshot)) = first.to_owned_message().payload else {
            anyhow::bail!("wrong initial event kind");
        };
        assert_eq!(snapshot.companions[0].id, "ene");
        let (http, config) = w::connect(&endpoint, "wrong-token")?;
        let error = w::HostServiceClient::new(http, config)
            .get_snapshot(w::Empty::default())
            .await
            .expect_err("a wrong token must be rejected");
        assert_eq!(error.code, connectrpc::ErrorCode::Unauthenticated);
        drop(stream);
        host.shutdown.cancel();
        shutdown.cancel();
        server.await??;
        actor.await?;
        Ok(())
    }
}
