//! Native ACP (Agent Client Protocol) stdio JSON-RPC 2.0 subprocess bridge (`F23`).
//!
//! Launches an explicit executable (`Target::Acp` plugin entrypoint command or
//! direct `AcpConnection` command) inside a `process-wrap` kill-on-drop group
//! (Windows Job Object / Unix Process Group), drives the ACP JSON-RPC 2.0
//! lifecycle (`initialize` -> `session/new` -> `session/prompt`), streams
//! `session/update` notifications as `ene_plugin::HarnessEvent`, and bridges
//! `session/request_permission` round-trips cleanly.
use crate::host::error::{HostError, Result};
use ene_plugin::{
    ContentBlock, HarnessEvent, PermissionOption, RequestPermissionOutcome, SessionUpdate,
    StopReason, ToolCallUpdate,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

const MAX_LINE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AcpLaunchSpec {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
}

/// Validates and spawns a managed ACP subprocess wrapped in `KillOnDrop` +
/// OS process group / Job Object so child trees never leak on cancellation.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn spawn_acp_process(
    spec: &AcpLaunchSpec,
) -> Result<Box<dyn process_wrap::tokio::ChildWrapper>> {
    let program = spec.command.trim();
    if program.is_empty() || program.chars().any(char::is_control) {
        return Err(HostError::Invalid(
            "ACP command executable must be non-empty without control characters".into(),
        ));
    }
    let mut cmd = process_wrap::tokio::CommandWrap::with_new(program, |cmd| {
        cmd.args(&spec.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(cwd) = spec.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            cmd.current_dir(Path::new(cwd));
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
    });
    #[cfg(unix)]
    cmd.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    cmd.wrap(process_wrap::tokio::JobObject);
    cmd.wrap(process_wrap::tokio::KillOnDrop);
    cmd.spawn()
        .map_err(|error| HostError::Unavailable(format!("ACP subprocess spawn failed: {error}")))
}

#[derive(Debug, Deserialize)]
struct JsonRpcIncoming {
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

/// Callback deciding an incoming `session/request_permission` RPC from the ACP
/// agent process.
pub(crate) type PermissionResponder = Box<
    dyn FnMut(String, ToolCallUpdate, Vec<PermissionOption>) -> RequestPermissionOutcome + Send,
>;

/// Bidirectional JSON-RPC 2.0 ACP client over any async byte stream (child
/// stdio in production, `tokio::io::duplex` in tests).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AcpBridge<R, W> {
    reader: BufReader<R>,
    writer: W,
    next_id: u64,
    session_id: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> AcpBridge<R, W> {
    pub(crate) fn new(reader: R, writer: W) -> Self {
        Self {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
            session_id: None,
        }
    }

    async fn send_json(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)
            .map_err(|e| HostError::Internal(format!("ACP JSON serialize: {e}")))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| HostError::Unavailable(format!("ACP write failed: {e}")))?;
        self.writer
            .flush()
            .await
            .map_err(|e| HostError::Unavailable(format!("ACP flush failed: {e}")))
    }

    async fn read_message(&mut self) -> Result<Option<JsonRpcIncoming>> {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .await
            .map_err(|e| HostError::Unavailable(format!("ACP read failed: {e}")))?;
        if n == 0 {
            return Ok(None);
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(HostError::Unavailable(
                "ACP JSON-RPC message exceeded size limit".into(),
            ));
        }
        serde_json::from_str::<JsonRpcIncoming>(line.trim())
            .map(Some)
            .map_err(|e| HostError::Unavailable(format!("invalid ACP JSON-RPC message: {e}")))
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        events: Option<&mpsc::Sender<HarnessEvent>>,
        mut on_permission: Option<&mut PermissionResponder>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let expected_id = json!(id);
        self.send_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;

        while let Some(msg) = self.read_message().await? {
            if let Some(incoming_method) = msg.method.as_deref() {
                match incoming_method {
                    "session/update" => {
                        if let Some(params) = msg.params
                            && let Some(update_val) = params.get("update").cloned()
                            && let Ok(update) = serde_json::from_value::<SessionUpdate>(update_val)
                            && let Some(tx) = events
                        {
                            let _ = tx.send(HarnessEvent::Update { update }).await;
                        }
                    }
                    "session/request_permission" => {
                        let req_id = msg.id.unwrap_or(Value::Null);
                        let params = msg.params.unwrap_or(Value::Null);
                        let req_id_str = req_id.to_string();
                        let tool_call = params
                            .get("toolCall")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<ToolCallUpdate>(v).ok())
                            .unwrap_or_else(|| ToolCallUpdate::new("unknown", Default::default()));
                        let options = params
                            .get("options")
                            .cloned()
                            .and_then(|v| serde_json::from_value::<Vec<PermissionOption>>(v).ok())
                            .unwrap_or_default();
                        let outcome = if let Some(handler) = on_permission.as_deref_mut() {
                            handler(req_id_str, tool_call, options)
                        } else {
                            RequestPermissionOutcome::Cancelled
                        };
                        self.send_json(&json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "result": { "outcome": outcome },
                        }))
                        .await?;
                    }
                    _ => {
                        // Ignore unknown notifications; return method-not-found for unknown requests.
                        if let Some(req_id) = msg.id {
                            self.send_json(&json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": { "code": -32601, "message": format!("unsupported method `{incoming_method}`") }
                            }))
                            .await?;
                        }
                    }
                }
                continue;
            }
            if msg.id.as_ref() == Some(&expected_id) {
                if let Some(err) = msg.error {
                    return Err(HostError::Unavailable(format!(
                        "ACP `{method}` returned error: {err}"
                    )));
                }
                return Ok(msg.result.unwrap_or(Value::Null));
            }
        }
        Err(HostError::Unavailable(format!(
            "ACP subprocess closed before answering `{method}`"
        )))
    }

    pub(crate) async fn initialize(&mut self) -> Result<Value> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": { "name": "ene-host", "version": env!("CARGO_PKG_VERSION") }
            }),
            None,
            None,
        )
        .await
    }

    pub(crate) async fn new_session(&mut self, cwd: &str) -> Result<String> {
        let result = self
            .request(
                "session/new",
                json!({ "cwd": cwd, "mcpServers": [] }),
                None,
                None,
            )
            .await?;
        let sid = result
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| HostError::Unavailable("ACP session/new returned no sessionId".into()))?
            .to_owned();
        self.session_id = Some(sid.clone());
        Ok(sid)
    }

    pub(crate) async fn prompt(
        &mut self,
        prompt: Vec<ContentBlock>,
        events: &mpsc::Sender<HarnessEvent>,
        mut on_permission: PermissionResponder,
    ) -> Result<StopReason> {
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| HostError::Conflict("ACP session has not been created".into()))?;
        let result = self
            .request(
                "session/prompt",
                json!({ "sessionId": session_id, "prompt": prompt }),
                Some(events),
                Some(&mut on_permission),
            )
            .await?;
        let stop = result
            .get("stopReason")
            .cloned()
            .and_then(|v| serde_json::from_value::<StopReason>(v).ok())
            .unwrap_or(StopReason::EndTurn);
        let _ = events.send(HarnessEvent::Stop { stop_reason: stop }).await;
        Ok(stop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_plugin::{ContentChunk, PermissionOptionKind, SelectedPermissionOutcome, text_block};

    #[tokio::test]
    // TODO(Phase 1-3): split during refactoring.
    #[allow(clippy::too_many_lines)]
    async fn t19_acp_stdio_jsonrpc_bridge_runs_initialize_session_prompt_and_permission() {
        let (client_read, mut agent_write) = tokio::io::duplex(16 * 1024);
        let (agent_read, client_write) = tokio::io::duplex(16 * 1024);

        // Simulated ACP subprocess speaking JSON-RPC 2.0 over stdio.
        let agent = tokio::spawn(async move {
            let mut lines = BufReader::new(agent_read).lines();
            // 1. initialize
            let init_req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(init_req["method"], "initialize");
            let init_resp = json!({
                "jsonrpc": "2.0",
                "id": init_req["id"],
                "result": { "protocolVersion": 1 }
            });
            agent_write
                .write_all(format!("{init_resp}\n").as_bytes())
                .await
                .unwrap();

            // 2. session/new
            let new_req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(new_req["method"], "session/new");
            let new_resp = json!({
                "jsonrpc": "2.0",
                "id": new_req["id"],
                "result": { "sessionId": "acp-sess-1" }
            });
            agent_write
                .write_all(format!("{new_resp}\n").as_bytes())
                .await
                .unwrap();

            // 3. session/prompt -> emit session/update, request permission, finish EndTurn.
            let prompt_req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(prompt_req["method"], "session/prompt");
            assert_eq!(prompt_req["params"]["sessionId"], "acp-sess-1");

            let update = SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block(
                "ACPエージェントが作業中です",
            )));
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": "acp-sess-1", "update": update }
            });
            agent_write
                .write_all(format!("{notif}\n").as_bytes())
                .await
                .unwrap();

            let perm_req = json!({
                "jsonrpc": "2.0",
                "id": "perm-42",
                "method": "session/request_permission",
                "params": {
                    "sessionId": "acp-sess-1",
                    "toolCall": ToolCallUpdate::new("call-1", Default::default()),
                    "options": [
                        PermissionOption::new("allow_once", "Allow", PermissionOptionKind::AllowOnce)
                    ]
                }
            });
            agent_write
                .write_all(format!("{perm_req}\n").as_bytes())
                .await
                .unwrap();

            let perm_reply: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(perm_reply["id"], "perm-42");
            assert_eq!(perm_reply["result"]["outcome"]["optionId"], "allow_once");

            let prompt_resp = json!({
                "jsonrpc": "2.0",
                "id": prompt_req["id"],
                "result": { "stopReason": StopReason::EndTurn }
            });
            agent_write
                .write_all(format!("{prompt_resp}\n").as_bytes())
                .await
                .unwrap();
        });

        let mut bridge = AcpBridge::new(client_read, client_write);
        bridge.initialize().await.unwrap();
        let sid = bridge.new_session("/workspace").await.unwrap();
        assert_eq!(sid, "acp-sess-1");

        let (tx, mut rx) = mpsc::channel(16);
        let stop = bridge
            .prompt(
                vec![text_block("テスト実行して")],
                &tx,
                Box::new(|_req_id, _tool_call, options| {
                    let chosen = options[0].option_id.clone();
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(chosen))
                }),
            )
            .await
            .unwrap();
        assert_eq!(stop, StopReason::EndTurn);
        drop(tx);

        let mut saw_chunk = false;
        let mut saw_stop = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                HarnessEvent::Update {
                    update: SessionUpdate::AgentMessageChunk(chunk),
                } => {
                    if matches!(chunk.content, ContentBlock::Text(t) if t.text.contains("ACPエージェント"))
                    {
                        saw_chunk = true;
                    }
                }
                HarnessEvent::Stop {
                    stop_reason: StopReason::EndTurn,
                } => saw_stop = true,
                _ => {}
            }
        }
        assert!(saw_chunk && saw_stop);
        agent.await.unwrap();

        // Reject invalid command spec cleanly.
        assert!(
            spawn_acp_process(&AcpLaunchSpec {
                command: String::new(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
            })
            .is_err()
        );
    }
}
