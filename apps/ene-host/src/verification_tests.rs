use crate::approval::ApprovalCommands as _;
use crate::configuration::ConfigurationCommands as _;
use crate::device::DeviceCommands as _;
use crate::memory::MemoryCommands as _;
use crate::voice::VoiceCapture as _;
use crate::voice::VoiceCommands as _;
use crate::work::WorkCommands as _;

use crate::{
    actor::{Actor, Host},
    approval::Operation,
    error::ApiError,
};
use anyhow::Result;
use connectrpc::ErrorCode;
use ene_core::policy::{Action, Effect, Target};
use ene_rpc::{self as w, event::Payload};

async fn start(path: &std::path::Path) -> Result<(Host, tokio::task::JoinHandle<()>)> {
    std::fs::create_dir_all(path.join("conversations"))?;
    Actor::start(path.to_owned()).await
}

fn mutation(
    key: &str,
    revision: i64,
) -> buffa::MessageField<w::Mutation, buffa::Inline<w::Mutation>> {
    Some(w::Mutation {
        request_key: key.into(),
        revision,
        ..Default::default()
    })
    .into()
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "requires an unlocked, isolated Linux Secret Service"]
async fn host_restarts_after_using_the_native_credential_store() -> Result<()> {
    let root = tempfile::tempdir()?;
    let account = crate::actor::id();
    let (host, running) = start(root.path()).await?;
    host.call({
        let account = account.clone();
        let secret = Some("fixture-secret".into());
        move |actor| Box::pin(actor.models.write_credential(account, secret))
    })
    .await?;
    host.shutdown.cancel();
    running.await?;

    let (host, running) = start(root.path()).await?;
    let value = tokio::task::spawn_blocking({
        let account = account.clone();
        move || crate::keyring::entry(&account)?.get_password()
    })
    .await??;
    host.call(move |actor| Box::pin(actor.models.write_credential(account, None)))
        .await?;
    host.shutdown.cancel();
    running.await?;
    assert_eq!(value, "fixture-secret");
    Ok(())
}

#[tokio::test]
async fn accepted_chat_is_replayed_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (host, running) = start(root.path()).await?;
    let request = w::SendRequest {
        companion_id: "ene".into(),
        text: "最初の発話".into(),
        mutation: mutation("chat-key", 0),
        ..Default::default()
    };
    let initial = host
        .call({
            let request = request.clone();
            move |actor| Box::pin(actor.send(request))
        })
        .await?;
    host.shutdown.cancel();
    running.await?;
    let (host, running) = start(root.path()).await?;
    let replay = host
        .call({
            let request = request.clone();
            move |actor| Box::pin(actor.send(request))
        })
        .await?;
    assert_eq!(replay.id, initial.id);
    let error = host
        .call(move |actor| {
            Box::pin(actor.send(w::SendRequest {
                text: "別の発話".into(),
                ..request
            }))
        })
        .await
        .expect_err("same key with different content conflicts");
    assert_eq!(
        error.downcast_ref::<ApiError>().map(|error| error.0),
        Some(ErrorCode::Aborted)
    );
    let history = host
        .call(|actor| {
            Box::pin(actor.history(w::HistoryRequest {
                companion_id: "ene".into(),
                limit: 30,
                ..Default::default()
            }))
        })
        .await?;
    assert_eq!(history.messages.len(), 1);
    assert_eq!(history.messages[0].text, "最初の発話");
    host.shutdown.cancel();
    running.await?;
    Ok(())
}

#[tokio::test]
async fn forgotten_memory_stays_searchable_and_erasure_removes_it() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (host, running) = start(root.path()).await?;
    let memory = host
        .call(|actor| {
            Box::pin(
                actor.change_memory(w::MemoryChange {
                    mutation: mutation("remember", 0),
                    operation: w::memory_change::Operation::Remember.into(),
                    memory: Some(w::Memory {
                        companion_id: "ene".into(),
                        text: "Hiking in alpine forests".into(),
                        importance: 10,
                        ..Default::default()
                    })
                    .into(),
                    ..Default::default()
                }),
            )
        })
        .await?;
    let forgotten = host
        .call(move |actor| {
            Box::pin(actor.change_memory(w::MemoryChange {
                mutation: mutation("forget", memory.revision),
                operation: w::memory_change::Operation::Forget.into(),
                memory: Some(memory).into(),
                ..Default::default()
            }))
        })
        .await?;
    let (implicit, explicit) = host
        .call(|actor| {
            Box::pin(async {
                Ok((
                    actor.recall("ene", "alpine").await?,
                    actor
                        .search_memories(w::MemoryQuery {
                            companion_id: "ene".into(),
                            query: "alpine".into(),
                            limit: 10,
                            ..Default::default()
                        })
                        .await?,
                ))
            })
        })
        .await?;
    assert!(implicit.is_empty());
    assert_eq!(explicit.items.len(), 1);
    host.call(move |actor| {
        Box::pin(actor.change_memory(w::MemoryChange {
            mutation: mutation("erase", forgotten.revision),
            operation: w::memory_change::Operation::Erase.into(),
            memory: Some(forgotten).into(),
            ..Default::default()
        }))
    })
    .await?;
    let explicit = host
        .call(|actor| {
            Box::pin(actor.search_memories(w::MemoryQuery {
                companion_id: "ene".into(),
                query: "alpine".into(),
                limit: 10,
                ..Default::default()
            }))
        })
        .await?;
    assert!(explicit.items.is_empty());
    host.shutdown.cancel();
    running.await?;
    Ok(())
}

#[tokio::test]
async fn approval_cannot_be_consumed_with_a_different_digest() -> Result<()> {
    let root = tempfile::tempdir()?;
    let (host, running) = start(root.path()).await?;
    let mut events = host
        .call(|actor| Box::pin(async { Ok(actor.events.subscribe()) }))
        .await?;
    let target = Target {
        receiver: "fixture".into(),
        revision: "rev1".into(),
        operation: "write_note".into(),
        effect: Effect::Write,
        device: None,
    };
    let action = Action::new(
        target,
        serde_json::from_value(serde_json::json!({"text":"a note"}))?,
    )?;
    let digest = action.digest()?;
    let confirmation = tokio::spawn({
        let host = host.clone();
        async move {
            let run = Operation {
                companion: "ene".into(),
                run: String::new(),
                brief: "write a note".into(),
                cancel: host.shutdown.child_token(),
            };
            host.authorize(&run, action).await
        }
    });
    let card = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events.recv().await?;
            if let Some(Payload::Approval(card)) = event.payload {
                break Ok::<_, anyhow::Error>(card);
            }
        }
    })
    .await??;
    assert_eq!(card.digest, digest);
    let request = w::ApprovalAnswer {
        mutation: mutation("answer", 0),
        id: card.id,
        choice: w::approval_answer::Choice::Allow.into(),
        digest: digest.clone(),
        ..Default::default()
    };
    let error = host
        .call({
            let request = request.clone();
            move |actor| {
                Box::pin(actor.answer(w::ApprovalAnswer {
                    digest: "modified".into(),
                    ..request
                }))
            }
        })
        .await
        .expect_err("the changed digest must not consume the card");
    assert_eq!(
        error.downcast_ref::<ApiError>().map(|error| error.0),
        Some(ErrorCode::Aborted)
    );
    assert!(!confirmation.is_finished());
    host.call(move |actor| Box::pin(actor.answer(request)))
        .await?;
    assert_eq!(confirmation.await??, digest);
    host.shutdown.cancel();
    running.await?;
    Ok(())
}

#[tokio::test]
async fn dialogue_uses_streamed_reply_and_keeps_dynamic_text_out_of_the_journal() -> Result<()> {
    use axum::{Router, routing::post};
    use std::sync::{Arc, Mutex};

    let body = Arc::new(Mutex::new(serde_json::Value::Null));
    let capture = body.clone();
    let router = Router::new().route("/v1/chat/completions", post(
        move |axum::Json(request): axum::Json<serde_json::Value>| {
            let capture = capture.clone();
            async move {
                *capture.lock().expect("recorded model request") = request;
                ([("content-type", "text/event-stream")], concat!(
                    "data: {\"id\":\"reply-1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
                    "\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
                    "\"content\":\"The streamed companion reply\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"reply-1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
                    "\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: [DONE]\n\n"
                ))
            }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1", listener.local_addr()?);
    let fixture = tokio::spawn(async move { axum::serve(listener, router).await });
    let root = tempfile::tempdir()?;
    let (host, running) = start(root.path()).await?;
    let mut events = host
        .call(move |actor| {
            Box::pin(async move {
                let connection = actor
                    .put_connection(w::ConnectionChange {
                        mutation: mutation("provider", 0),
                        connection: Some(w::Connection {
                            name: "fixture".into(),
                            endpoint,
                            enabled: true,
                            kind: w::ProviderKind::OpenaiCompatible.into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                let mut companion = actor.companion("ene").await?;
                companion.models.push(w::ModelSelection {
                    role: w::model_selection::Role::Dialogue.into(),
                    connection_id: connection.id,
                    model: "fixture".into(),
                    ..Default::default()
                });
                actor
                    .put_companion(w::CompanionChange {
                        mutation: mutation("companion", companion.revision),
                        companion: Some(companion).into(),
                        ..Default::default()
                    })
                    .await?;
                actor
                    .change_memory(w::MemoryChange {
                        mutation: mutation("secret-memory", 0),
                        operation: w::memory_change::Operation::Remember.into(),
                        memory: Some(w::Memory {
                            companion_id: "ene".into(),
                            text: "pinned memory for this prompt only".into(),
                            importance: 10,
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                Ok(actor.events.subscribe())
            })
        })
        .await?;
    let generation = host
        .call(|actor| {
            Box::pin(async move {
                actor
                    .register_device(w::DeviceChange {
                        mutation: mutation("dialogue-device", 0),
                        device: Some(w::Device {
                            id: "fixture-desktop".into(),
                            eyes_enabled: true,
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                let active = actor
                    .activate(w::ActivationRequest {
                        companion_id: "ene".into(),
                        mutation: mutation("dialogue-active", 0),
                        device_id: "fixture-desktop".into(),
                        ..Default::default()
                    })
                    .await?;
                actor
                    .perception(w::Perception {
                        device_id: active.device_id,
                        generation: active.generation,
                        observation: "foreground fixture only".into(),
                        interpretation: "fixture interpretation".into(),
                        ..Default::default()
                    })
                    .await?;
                Ok(active.generation)
            })
        })
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while !host
            .call(|actor| {
                Box::pin(async {
                    Ok(actor
                        .observations
                        .get("ene")
                        .is_some_and(|current| current.value.is_some()))
                })
            })
            .await?
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    host.call(|actor| {
        Box::pin(actor.send(w::SendRequest {
            mutation: mutation("dialogue", 0),
            companion_id: "ene".into(),
            text: "Hello".into(),
            ..Default::default()
        }))
    })
    .await?;
    let (reply, delta) = tokio::time::timeout(std::time::Duration::from_secs(6), async {
        let mut delta = String::new();
        loop {
            match events.recv().await?.payload {
                Some(Payload::Delta(part)) => delta.push_str(&part.text),
                Some(Payload::Message(message)) if message.role == w::message::Role::Assistant => {
                    break Ok::<_, anyhow::Error>((message.text, delta));
                }
                Some(Payload::Turn(turn)) if turn.outcome == w::Outcome::Failed => {
                    anyhow::bail!("model turn failed")
                }
                _ => {}
            }
        }
    })
    .await??;
    assert_eq!(reply, "The streamed companion reply");
    assert_eq!(reply, delta);
    let actual = body.lock().expect("captured request").to_string();
    assert!(actual.contains("pinned memory for this prompt only"));
    assert!(actual.contains("[Current context]"));
    assert!(actual.contains("foreground fixture only"));
    assert!(actual.contains("fixture interpretation"));
    let journal = std::fs::read_to_string(root.path().join("conversations/ene.jsonl"))?;
    assert!(!journal.contains("pinned memory for this prompt only"));
    assert!(!journal.contains("[Current context]"));
    assert!(!journal.contains("foreground fixture only"));
    host.call(move |actor| {
        Box::pin(async move {
            actor
                .perception(w::Perception {
                    device_id: "fixture-desktop".into(),
                    generation,
                    ..Default::default()
                })
                .await?;
            assert!(
                actor
                    .observations
                    .get("ene")
                    .is_none_or(|current| current.value.is_none())
            );
            Ok(())
        })
    })
    .await?;
    host.shutdown.cancel();
    running.await?;
    fixture.abort();
    Ok(())
}

#[tokio::test]
async fn screen_capture_reaches_the_work_model_as_a_private_tool_image() -> Result<()> {
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};
    use std::time::Duration;

    let router = Router::new().route("/v1/chat/completions", post(|Json(request): Json<Value>| async move {
        let result = request["messages"].as_array().is_some_and(|messages| {
            messages.iter().any(|message| {
                message["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| part["image_url"]["url"] == "data:image/jpeg;base64,/9j/2Q==")
                })
            })
        });
        let choice = if result {
            json!({"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"Verified the screen image."}})
        } else if request["messages"].as_array().is_some_and(|messages| messages.iter().any(|m| m["role"] == "tool")) {
            json!({"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"Image not received."}})
        } else {
            assert!(request["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "desktop")));
            json!({"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","tool_calls":[
                {"id":"capture","type":"function","function":{"name":"desktop","arguments":"{\"action\":\"screenshot\"}"}}
            ]}})
        };
        Json(json!({"id":"fixture","object":"chat.completion","created":1,"model":"fixture","choices":[choice]}))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1", listener.local_addr()?);
    let fixture = tokio::spawn(async move { axum::serve(listener, router).await });
    let root = tempfile::tempdir()?;
    let (host, actor) = start(root.path()).await?;
    let (mut events, mut operations, generation) = host.call(move |actor| Box::pin(async move {
        let connection = actor.put_connection(w::ConnectionChange {
            mutation: mutation("provider", 0),
            connection: Some(w::Connection {
                name: "fixture".into(), endpoint, enabled: true, kind: w::ProviderKind::OpenaiCompatible.into(), ..Default::default()
            }).into(),
            ..Default::default()
        }).await?;
        let mut companion = actor.companion("ene").await?;
        companion.models.push(w::ModelSelection {
            role: w::model_selection::Role::Work.into(), connection_id: connection.id, model: "fixture".into(),
            ..Default::default()
        });
        actor.put_companion(w::CompanionChange {
            mutation: mutation("companion", companion.revision), companion: Some(companion).into(), ..Default::default()
        }).await?;
        actor.register_device(w::DeviceChange {
            mutation: mutation("device", 0), device: Some(w::Device {
                id: "test-display".into(),
                capabilities: vec![w::Capability {
                    name: "desktop.input".into(),
                    schema_json: json!({"type":"object","properties":{"action":{"const":"screenshot"}},"required":["action"],"additionalProperties":false}).to_string(),
                    ..Default::default()
                }], ..Default::default()
            }).into(), ..Default::default()
        }).await?;
        let active = actor.activate(w::ActivationRequest {
                        companion_id: "ene".into(),
            mutation: mutation("activate", 0), device_id: "test-display".into(), ..Default::default()
        }).await?;
        Ok((actor.events.subscribe(), actor.operations.subscribe(), active.generation))
    })).await?;

    let receipt = host
        .call(|actor| {
            Box::pin(actor.send_work(w::WorkInput {
                mutation: mutation("screen-work", 0),
                companion_id: "ene".into(),
                instruction: "Capture my current screen.".into(),
                ..Default::default()
            }))
        })
        .await?;
    let approval = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Payload::Approval(card)) = events.recv().await?.payload {
                return Ok::<_, anyhow::Error>(card);
            }
        }
    })
    .await??;
    assert_eq!(approval.run_id, receipt.id);
    assert!(operations.try_recv().is_err());
    host.call(move |actor| {
        Box::pin(actor.answer(w::ApprovalAnswer {
            mutation: mutation("allow-screen", 0),
            id: approval.id,
            digest: approval.digest,
            choice: w::approval_answer::Choice::Allow.into(),
            ..Default::default()
        }))
    })
    .await?;
    let command = tokio::time::timeout(Duration::from_secs(5), operations.recv()).await??;
    assert_eq!(command.run_id, receipt.id);
    let frame_id = uuid::Uuid::new_v4().to_string();
    host.call(move |actor| {
        Box::pin(async move {
            let claim = w::DeviceClaim {
                device_id: "test-display".into(),
                operation_id: command.id.clone(),
                generation,
                ..Default::default()
            };
            actor.claim_device(claim.clone())?;
            assert!(actor.claim_device(claim).is_err());
            actor.complete_device(w::DeviceResult {
                device_id: "test-display".into(),
                operation_id: command.id,
                generation,
                outcome: w::Outcome::Completed.into(),
                result_json: json!({"frame_id":frame_id,"width":1,"height":1}).to_string(),
                frame: Some(w::ScreenFrame {
                    id: frame_id,
                    width: 1,
                    height: 1,
                    jpeg: vec![255, 216, 255, 217],
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            })
        })
    })
    .await?;
    let output = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Payload::Work(work)) = events.recv().await?.payload
                && let Some(run) = work.runs.iter().find(|run| {
                    run.id == receipt.id
                        && run.outcome != w::Outcome::Running
                        && run.outcome != w::Outcome::Unspecified
                })
            {
                return Ok::<_, anyhow::Error>((run.outcome, run.report.clone()));
            }
        }
    })
    .await??;
    assert_eq!(output.0, w::Outcome::Completed);
    assert_eq!(output.1, "Verified the screen image.");
    let journal = root.path().join("conversations/ene.jsonl");
    if journal.exists() {
        assert!(!std::fs::read_to_string(journal)?.contains("/9j/2Q=="));
    }
    host.shutdown.cancel();
    actor.await?;
    fixture.abort();
    Ok(())
}

#[tokio::test]
async fn voice_provider_transcribes_input_and_streams_pcm_to_the_active_device() -> Result<()> {
    use axum::{Json, Router, body::Bytes, routing::post};
    use serde_json::{Value, json};
    use std::time::Duration;

    let router = Router::new()
        .route(
            "/v1/audio/transcriptions",
            post(|body: Bytes| async move {
                assert!(body.windows(4).any(|window| window == b"RIFF"));
                Json(json!({"text":"The microphone fixture"}))
            }),
        )
        .route(
            "/v1/audio/speech",
            post(|Json(request): Json<Value>| async move {
                assert_eq!(request["input"], "The speaker fixture");
                assert_eq!(request["voice"], "fixture-voice");
                assert_eq!(request["response_format"], "pcm");
                (
                    [("content-type", "application/octet-stream")],
                    vec![1_u8, 0, 2, 0],
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1", listener.local_addr()?);
    let fixture = tokio::spawn(async move { axum::serve(listener, router).await });
    let root = tempfile::tempdir()?;
    let (host, actor) = start(root.path()).await?;
    let (generation, mut audio) = host
        .call(move |actor| {
            Box::pin(async move {
                let connection = actor
                    .put_connection(w::ConnectionChange {
                        mutation: mutation("voice-provider", 0),
                        connection: Some(w::Connection {
                            name: "voice fixture".into(),
                            endpoint,
                            enabled: true,
                            kind: w::ProviderKind::OpenaiCompatible.into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                let mut companion = actor.companion("ene").await?;
                let mut character = actor.snapshot().await?.characters.remove(0);
                character.voice = "fixture-voice".into();
                actor
                    .put_character(w::CharacterChange {
                        mutation: mutation("voice-character", character.revision),
                        character: Some(character).into(),
                        ..Default::default()
                    })
                    .await?;
                companion.models.extend(
                    [
                        w::model_selection::Role::Synthesis,
                        w::model_selection::Role::Transcription,
                    ]
                    .map(|role| w::ModelSelection {
                        role: role.into(),
                        connection_id: connection.id.clone(),
                        model: "fixture".into(),
                        ..Default::default()
                    }),
                );
                actor
                    .put_companion(w::CompanionChange {
                        mutation: mutation("voice-companion", companion.revision),
                        companion: Some(companion).into(),
                        ..Default::default()
                    })
                    .await?;
                actor
                    .register_device(w::DeviceChange {
                        mutation: mutation("voice-device", 0),
                        device: Some(w::Device {
                            id: "test-audio".into(),
                            ..Default::default()
                        })
                        .into(),
                        ..Default::default()
                    })
                    .await?;
                let active = actor
                    .activate(w::ActivationRequest {
                        companion_id: "ene".into(),
                        mutation: mutation("voice-activate", 0),
                        device_id: "test-audio".into(),
                        ..Default::default()
                    })
                    .await?;
                Ok((active.generation, actor.audio.subscribe()))
            })
        })
        .await?;
    let input = host
        .transcribe(crate::voice::AudioCapture {
            device: "test-audio".into(),
            generation,
            rate: 16_000,
            pcm: vec![1, 0, 2, 0],
            request_key: "test-mic".into(),
            diagnostic: false,
        })
        .await?;
    assert_eq!(input.text, "The microphone fixture");
    assert!(!input.id.is_empty());
    assert!(
        host.transcribe(crate::voice::AudioCapture {
            device: "test-audio".into(),
            generation: generation + 1,
            rate: 16_000,
            pcm: vec![1, 0, 2, 0],
            request_key: "stale-mic".into(),
            diagnostic: false,
        })
        .await
        .is_err()
    );
    host.call(|actor| {
        Box::pin(actor.speak(w::SpeakRequest {
            mutation: mutation("voice-speak", 0),
            companion_id: "ene".into(),
            text: "The speaker fixture".into(),
            ..Default::default()
        }))
    })
    .await?;
    let (first, end) = tokio::time::timeout(Duration::from_secs(5), async {
        let first = loop {
            let item = audio.recv().await?;
            if !item.pcm.is_empty() {
                break item;
            }
        };
        let end = audio.recv().await?;
        Ok::<_, anyhow::Error>((first, end))
    })
    .await??;
    assert_eq!(first.pcm, vec![1, 0, 2, 0]);
    assert_eq!(first.sample_rate, 24_000);
    assert_eq!(first.generation, generation);
    assert!(end.end);
    assert_eq!(first.utterance_id, end.utterance_id);
    host.shutdown.cancel();
    actor.await?;
    fixture.abort();
    Ok(())
}

#[path = "parity_tests.rs"]
mod parity;
