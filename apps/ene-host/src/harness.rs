use crate::{
    actor::Host,
    approval::Operation,
    error::{conflict, denied, missing},
    model::truncate,
    plugin_runtime::{
        Package,
        instance::{DenyServices, Instance},
    },
    store,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client,
    schema::ProtocolVersion,
    v1::{ConnectionTo, schema::*},
};
use anyhow::{Context, Result, ensure};
use ene_core::policy::{Action, Effect, Target};
use ene_plugin::{Event, Extension, HarnessEvent, HarnessInput};
use ene_rpc::{Connection, Plugin};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, LazyLock},
};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};

static RUNNING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

struct Slot(String);

impl Slot {
    fn acquire(id: String) -> Result<Self> {
        if !RUNNING.lock().insert(id.clone()) {
            return Err(conflict(
                "the external harness already has an active session",
            ));
        }
        Ok(Self(id))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        RUNNING.lock().remove(&self.0);
    }
}

pub(crate) async fn try_run(
    host: &Host,
    operation: &Operation,
    connection: &Connection,
    history: &str,
    session_id: &str,
) -> Result<Option<String>> {
    let plugin_id = connection.plugin_id.clone();
    let (plugin, cwd) = host
        .call(move |actor| {
            Box::pin(async move {
                let mut db = actor.db.acquire().await?;
                let plugin = store::get::<Plugin>(&mut db, "plugin", &plugin_id)
                    .await?
                    .filter(|plugin| plugin.enabled)
                    .ok_or_else(|| missing("harness plugin not enabled"))?;
                Ok((plugin, actor.state.join("work")))
            })
        })
        .await?;
    let path = PathBuf::from(&plugin.path);
    let package = tokio::task::spawn_blocking(move || Package::load(&path)).await??;
    ensure!(
        package.revision == plugin.package_digest,
        "harness manifest changed"
    );
    let entry = package.entry(&connection.entrypoint)?.clone();
    if !entry.provides.contains(&Extension::Harness) {
        return Ok(None);
    }
    let binding = store::fingerprint(&(&plugin.id, plugin.revision, &package.revision, &entry.id))?;
    let _slot = Slot::acquire(binding.clone())?;
    let session_key =
        store::fingerprint(&(session_id, &binding, &connection.id, connection.revision))?;
    let permission = HarnessPermission {
        host: host.clone(),
        operation: operation.clone(),
        receiver: format!("plugin/{}/{}", plugin.id, entry.id),
        revision: binding,
    };
    let action = permission.action(
        "harness.prompt",
        serde_json::from_value(json!({"instruction":operation.brief,"session":session_id}))?,
    )?;
    let digest = host.authorize(operation, action.clone()).await?;
    action.verify(&digest)?;
    tokio::fs::create_dir_all(&cwd).await?;
    let cwd = tokio::fs::canonicalize(cwd).await?;
    let prompt = if history.is_empty() {
        operation.brief.clone()
    } else {
        format!(
            "Previous work in this session:\n{history}\nNew instruction:\n{}",
            operation.brief
        )
    };
    let result = tokio::select! {
        biased;
        () = operation.cancel.cancelled() => return Err(denied("harness cancelled")),
        result = async {
            match entry.target {
                ene_plugin::Target::Acp => {
                    let saved = host.call({
                        let key = session_key.clone();
                        move |actor| Box::pin(async move {
                            let mut db = actor.db.acquire().await?;
                            store::get::<String>(&mut db, "acp_session", &key).await
                        })
                    }).await?;
                    let (report, resumable) = acp(permission, cwd, &entry.command, prompt, saved.clone()).await?;
                    if saved.is_none() && let Some(resumable) = resumable {
                        host.call(move |actor| Box::pin(async move {
                            let mut db = actor.db.acquire().await?;
                            store::put(&mut db, "acp_session", &session_key, 0, &resumable).await
                        })).await?;
                    }
                    Ok(report)
                },
                ene_plugin::Target::Host => wasm(permission, package, entry.id, cwd, prompt).await,
                ene_plugin::Target::Client => Err(denied("harness is declared for a different target")),
            }
        } => result?,
    };
    Ok(Some(result))
}

#[derive(Clone)]
struct HarnessPermission {
    host: Host,
    operation: Operation,
    receiver: String,
    revision: String,
}

impl HarnessPermission {
    fn action(&self, name: &str, arguments: Map<String, Value>) -> Result<Action> {
        Action::new(
            Target {
                receiver: self.receiver.clone(),
                revision: self.revision.clone(),
                operation: name.into(),
                effect: Effect::Execute,
                device: None,
            },
            arguments,
        )
    }

    async fn answer(
        &self,
        call: ToolCallUpdate,
        options: &[PermissionOption],
    ) -> RequestPermissionOutcome {
        let decision = async {
            let name = call
                .fields
                .name
                .as_deref()
                .or(call.fields.title.as_deref())
                .context("tool name missing")?;
            let mut args = call
                .fields
                .raw_input
                .and_then(|value| value.as_object().cloned())
                .context("tool lacks a bound input")?;
            args.insert("tool_call_id".into(), json!(call.tool_call_id.to_string()));
            if let Some(locations) = call.fields.locations {
                args.insert("locations".into(), serde_json::to_value(locations)?);
            }
            let action = self.action(name, args)?;
            let digest = self.host.authorize(&self.operation, action.clone()).await?;
            action.verify(&digest)
        }
        .await;
        let kind = if decision.is_ok() {
            PermissionOptionKind::AllowOnce
        } else {
            PermissionOptionKind::RejectOnce
        };
        options
            .iter()
            .find(|option| option.kind == kind)
            .map(|option| {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                    option.option_id.clone(),
                ))
            })
            .unwrap_or(RequestPermissionOutcome::Cancelled)
    }
}

async fn acp(
    permission: HarnessPermission,
    cwd: PathBuf,
    command: &[String],
    prompt: String,
    saved: Option<String>,
) -> Result<(String, Option<String>)> {
    let (executable, args) = command.split_first().context("harness command required")?;
    let (_child, input, output) = crate::process::spawn(executable, args)?;
    let transport = ByteStreams::new(input.compat_write(), output.compat());
    let collected = Arc::new(Mutex::new(String::new()));
    let target = collected.clone();
    let current_report = collected.clone();
    let next_instruction = permission.operation.brief.clone();
    Client
        .v1()
        .on_receive_notification(
            async move |event: SessionNotification, _| {
                append(&mut target.lock(), event.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _| {
                let result = permission.answer(request.tool_call, &request.options).await;
                responder.respond(RequestPermissionResponse::new(result))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, move |client: ConnectionTo<Agent>| async move {
            let init = client
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let can_resume = init.agent_capabilities.load_session;
            let (session, prompt) = if let Some(saved) = saved.filter(|_| can_resume) {
                let session = SessionId::new(saved);
                client
                    .send_request(LoadSessionRequest::new(session.clone(), cwd))
                    .block_task()
                    .await?;
                current_report.lock().clear();
                (session, next_instruction)
            } else {
                let session = client
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?
                    .session_id;
                (session, prompt)
            };
            let resume = can_resume.then(|| session.to_string());
            let reply = client
                .send_request(PromptRequest::new(
                    session,
                    vec![ContentBlock::Text(TextContent::new(prompt))],
                ))
                .block_task()
                .await?;
            Ok((reply.stop_reason, resume))
        })
        .await
        .map_err(|_| anyhow::anyhow!("ACP connection failed"))
        .and_then(|(reason, resume)| {
            ensure!(reason == StopReason::EndTurn, "ACP work did not complete");
            Ok((std::mem::take(&mut *collected.lock()), resume))
        })
}

async fn wasm(
    permission: HarnessPermission,
    package: Package,
    entry: String,
    cwd: PathBuf,
    prompt: String,
) -> Result<String> {
    let cancel = &permission.operation.cancel;
    let mut instance = Instance::load(package, entry, cancel).await?;
    let mut input = HarnessInput::Start {
        prompt: vec![ContentBlock::Text(TextContent::new(prompt))],
        cwd: cwd.to_string_lossy().into_owned(),
    };
    let mut report = String::new();
    for _ in 0..16 {
        let (sender, mut events) = mpsc::channel(32);
        let work = instance.invoke(
            ene_plugin::Operation::Harness { input },
            Arc::new(DenyServices),
            sender,
            cancel,
        );
        let receive = async {
            let mut terminal = None;
            while let Some(event) = events.recv().await {
                if let Event::Harness { event } = event {
                    match event {
                        HarnessEvent::Update { update } => append(&mut report, update),
                        other => terminal = Some(other),
                    }
                }
            }
            terminal
        };
        let (result, terminal) = tokio::join!(work, receive);
        result?;
        match terminal {
            Some(HarnessEvent::Stop {
                stop_reason: StopReason::EndTurn,
            }) => return Ok(report),
            Some(HarnessEvent::RequestPermission {
                request_id,
                tool_call,
                options,
            }) => {
                let outcome = permission.answer(tool_call, &options).await;
                input = HarnessInput::Respond {
                    request_id,
                    outcome,
                };
            }
            _ => anyhow::bail!("Wasm harness ended without a successful outcome"),
        }
    }
    anyhow::bail!("harness exceeded permission limit")
}

fn append(text: &mut String, update: SessionUpdate) {
    if let SessionUpdate::AgentMessageChunk(chunk) = update
        && let ContentBlock::Text(delta) = chunk.content
    {
        text.push_str(truncate(
            &delta.text,
            24_000usize.saturating_sub(text.len()),
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    #[ignore = "runs a Python ACP fixture as an external process"]
    async fn acp_resumes_the_same_session_and_excludes_replayed_output() -> Result<()> {
        let home = tempfile::tempdir()?;
        std::fs::create_dir_all(home.path().join("conversations"))?;
        let (host, actor) = crate::actor::Actor::start(home.path().to_owned()).await?;
        let command: Vec<String> = [
            "python3",
            "-u",
            "-c",
            r#"
import json, sys
def send(message):
    print(json.dumps(dict(jsonrpc='2.0', **message)), flush=True)
def delta(session, text):
    send(dict(method='session/update', params=dict(sessionId=session, update=dict(
        sessionUpdate='agent_message_chunk', content=dict(type='text', text=text)))))
session = None
for line in sys.stdin:
    request = json.loads(line)
    method = request['method']
    params = request.get('params', {})
    if method == 'initialize':
        result = dict(protocolVersion=1, agentCapabilities=dict(loadSession=True))
    elif method == 'session/new':
        session = 'retained-session'
        result = dict(sessionId=session)
    elif method == 'session/load':
        session = params['sessionId']
        assert session == 'retained-session'
        delta(session, 'old reply, not this run')
        result = {}
    elif method == 'session/prompt':
        assert session == params['sessionId']
        delta(session, params['prompt'][0]['text'])
        result = dict(stopReason='end_turn')
    else:
        raise RuntimeError(method)
    send(dict(id=request['id'], result=result))
"#,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let permission = HarnessPermission {
            host: host.clone(),
            operation: Operation {
                companion: "ene".into(),
                run: "second-run".into(),
                brief: "new instruction only".into(),
                cancel: CancellationToken::new(),
            },
            receiver: "fixture".into(),
            revision: "1".into(),
        };
        let cwd = home.path().canonicalize()?;
        let (first, saved) = acp(
            permission.clone(),
            cwd.clone(),
            &command,
            "first prompt".into(),
            None,
        )
        .await?;
        assert_eq!(first, "first prompt");
        assert_eq!(saved.as_deref(), Some("retained-session"));
        let (second, _) = acp(
            permission,
            cwd,
            &command,
            "previous context plus new instructions".into(),
            saved,
        )
        .await?;
        assert_eq!(second, "new instruction only");
        host.shutdown.cancel();
        actor.await?;
        Ok(())
    }
}
