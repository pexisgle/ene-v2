//! WebSocket upgrade and bounded audio frame exchange.
use crate::api::{
    auth::{self, Principal},
    state::App,
};
use axum::{
    extract::{
        Extension, State, WebSocketUpgrade,
        ws::{Message as Frame, WebSocket},
    },
    response::Response,
};
use buffa::Message;
use ene_rpc::rpc as w;
use futures_util::SinkExt;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) async fn upgrade(
    State(app): State<App>,
    Extension(principal): Extension<Principal>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| stream(app, principal, socket))
}
async fn send(socket: &mut WebSocket, frame: w::AudioFrame) -> bool {
    if frame.encoded_len() > 64 * 1024 {
        return false;
    }
    matches!(
        tokio::time::timeout(
            Duration::from_secs(2),
            socket.send(Frame::Binary(frame.encode_to_vec().into()))
        )
        .await,
        Ok(Ok(()))
    )
}
async fn fail(socket: &mut WebSocket, id: &str, code: &str) {
    let _ = send(
        socket,
        w::AudioFrame {
            frame: Some(w::audio_frame::Frame::Failure(Box::new(w::AudioFailure {
                session_id: id.into(),
                code: code.into(),
                ..Default::default()
            }))),
            ..Default::default()
        },
    )
    .await;
}
async fn stream(app: App, principal: Principal, mut socket: WebSocket) {
    let incoming = tokio::time::timeout(Duration::from_secs(5), socket.recv()).await;
    let Ok(Some(Ok(Frame::Binary(bytes)))) = incoming else {
        return;
    };
    let Ok(frame) = w::AudioFrame::decode_from_slice(&bytes) else {
        fail(&mut socket, "", "invalid_request").await;
        return;
    };
    let Some(w::audio_frame::Frame::Start(start)) = frame.frame else {
        fail(&mut socket, "", "invalid_request").await;
        return;
    };
    if matches!(&principal,Principal::Device(id, _) if *id!=start.device_id) {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    if app
        .devices
        .permitted(&start.device_id, "audio.input")
        .await
        .is_err()
        || app
            .devices
            .permitted(&start.device_id, "audio.output")
            .await
            .is_err()
    {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    if app
        .application
        .companion(&start.companion_id)
        .await
        .is_err()
    {
        fail(&mut socket, &start.session_id, "not_found").await;
        return;
    }
    let Ok(mut permission) = app.devices.changes(&start.device_id).await else {
        return;
    };
    if app
        .devices
        .permitted(&start.device_id, "audio.input")
        .await
        .is_err()
        || app
            .devices
            .permitted(&start.device_id, "audio.output")
            .await
            .is_err()
    {
        fail(&mut socket, &start.session_id, "permission_required").await;
        return;
    }
    let (_lease, mut cancelled) = match app.audio.reserve(&start).await {
        Ok(v) => v,
        Err(code) => {
            fail(&mut socket, &start.session_id, code).await;
            return;
        }
    };
    let Some(engine) = &app.audio.engine else {
        fail(&mut socket, &start.session_id, "unsupported_capability").await;
        return;
    };
    if !send(
        &mut socket,
        w::AudioFrame {
            frame: Some(w::audio_frame::Frame::Start(start.clone())),
            ..Default::default()
        },
    )
    .await
    {
        return;
    }
    let (input, inputs) = mpsc::channel(8);
    let mut input = Some(input);
    let (output, mut outputs) = mpsc::channel(8);
    let engine_task =
        app.audio
            .tasks
            .spawn(engine.run((*start).clone(), inputs, output, cancelled.clone()));
    let device_session = matches!(principal, Principal::Device(_, _));
    let cancelled_session = auth::cancelled(principal);
    tokio::pin!(cancelled_session);
    let mut shutdown = app.application.shutdown();
    let mut pending_input = None;
    let mut input_deadline = tokio::time::Instant::now();
    let mut expected = 0u64;
    let mut output_sequence = 0u64;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(3600);
    let reason = loop {
        if *cancelled.borrow() || *shutdown.borrow() {
            break "cancelled";
        }
        let queued_input = input.clone();
        tokio::select! {biased;
            _=&mut cancelled_session=>break if device_session {"permission_required"} else {"unauthorized"},
            _=tokio::time::sleep_until(deadline)=>break "timeout",
            _=shutdown.changed()=>break "cancelled",
            _=cancelled.changed()=>break "cancelled",
            _=permission.changed()=>break "permission_required",
            ready = async move {
                match queued_input {
                    Some(sender) => tokio::time::timeout_at(input_deadline, sender.reserve_owned()).await,
                    None => std::future::pending().await,
                }
            }, if pending_input.is_some() => match ready {
                Ok(Ok(permit)) => { permit.send(pending_input.take().expect("pending input")); },
                _ => break "resource_limit",
            },
            incoming=socket.recv(), if pending_input.is_none()=>match incoming {
                Some(Ok(Frame::Binary(bytes)))=> {
                    let Ok(frame)=w::AudioFrame::decode_from_slice(&bytes) else {break "invalid_request";};
                    match frame.frame {
                        Some(w::audio_frame::Frame::Chunk(chunk)) if input.is_some() && chunk.session_id==start.session_id && chunk.sequence==expected && !chunk.data.is_empty() && chunk.data.len()<=16*1024 && chunk.data.len().is_multiple_of(2) => {
                            if *cancelled.borrow() {break "cancelled";}
                            let Some(next)=expected.checked_add(1) else {break "resource_limit";};expected=next;
                            pending_input = Some(*chunk);
                            input_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                        }
                        Some(w::audio_frame::Frame::End(end)) if end.session_id==start.session_id && input.is_some() => {input.take();deadline=tokio::time::Instant::now()+Duration::from_secs(30);},
                        _=>break "invalid_request",
                    }
                }
                Some(Ok(Frame::Ping(data)))=> {if !matches!(tokio::time::timeout(Duration::from_secs(2),socket.send(Frame::Pong(data))).await,Ok(Ok(()))) {break "disconnected";}},
                Some(Ok(Frame::Pong(_)))=>{},
                _=>break "disconnected",
            },
            output=outputs.recv()=>match output {
                Some(frame)=> {
                    let valid=match &frame.frame {
                        Some(w::audio_frame::Frame::Chunk(chunk))=>chunk.session_id==start.session_id && chunk.sequence==output_sequence && !chunk.data.is_empty() && chunk.data.len()<=16*1024 && chunk.data.len().is_multiple_of(2),
                        Some(w::audio_frame::Frame::End(end))=>end.session_id==start.session_id,
                        Some(w::audio_frame::Frame::Failure(error))=>error.session_id==start.session_id,
                        _=>false,
                    };
                    if !valid {break "invalid_engine_output";}
                    if matches!(frame.frame,Some(w::audio_frame::Frame::Chunk(_))) {let Some(next)=output_sequence.checked_add(1) else {break "resource_limit";};output_sequence=next;}
                    if *cancelled.borrow() {break "cancelled";}
                    // Stop/revoke can interrupt a slow output send.
                    tokio::select! {biased;
                        _=cancelled.changed()=>break "cancelled",
                        _=permission.changed()=>break "permission_required",
                        sent=send(&mut socket,frame)=>if !sent {break "disconnected";}
                    }
                }
                None=>break "ended",
            }
        }
    };
    drop(input);
    engine_task.abort();
    let _ = engine_task.await;
    if reason == "ended" {
        let _ = send(
            &mut socket,
            w::AudioFrame {
                frame: Some(w::audio_frame::Frame::End(Box::new(w::AudioEnd {
                    session_id: start.session_id.clone(),
                    ..Default::default()
                }))),
                ..Default::default()
            },
        )
        .await;
    } else if reason != "disconnected" {
        fail(&mut socket, &start.session_id, reason).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
}
