//! Pure plugin-hand harness protocol buffering, progress formatting, and permission helpers.

use ene_domain::RunState;
use ene_plugin::{
    ContentBlock, PermissionOption, PermissionOptionKind, PlanEntryStatus,
    RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate, StopReason, ToolCallStatus,
    ToolCallUpdate, text_block,
};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Outcome of applying a harness [`SessionUpdate`] to [`HarnessBuffer`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateEffects {
    pub flushed: Option<String>,
    pub report: Option<String>,
}

/// Accumulates streamed harness message chunks and remembers reported tool names across updates.
#[derive(Clone, Debug, Default)]
pub struct HarnessBuffer {
    message: Option<(Option<String>, String)>,
    names: HashMap<String, String>,
}

impl HarnessBuffer {
    /// Applies one harness session update, returning any flushed prior message and progress report.
    pub fn apply_update(&mut self, update: &SessionUpdate) -> UpdateEffects {
        let chunk = matches!(update, SessionUpdate::AgentMessageChunk(_));
        let flushed = if chunk {
            None
        } else {
            self.message.take().map(|(_, text)| text)
        };
        let report = self.progress(update);
        UpdateEffects { flushed, report }
    }

    /// Takes the currently buffered message text, if any.
    pub fn take_message(&mut self) -> Option<String> {
        self.message.take().map(|(_, text)| text)
    }

    /// Clears any buffered message text.
    pub fn clear_message(&mut self) {
        self.message = None;
    }

    /// Resolves the model-facing tool name for `tool_call`, using prior name registrations when omitted.
    pub fn resolve_tool_name(&self, tool_call: &ToolCallUpdate) -> String {
        tool_call
            .fields
            .name
            .clone()
            .or_else(|| self.names.get(&tool_call.tool_call_id.to_string()).cloned())
            .unwrap_or_else(|| tool_call.tool_call_id.to_string())
    }

    fn progress(&mut self, update: &SessionUpdate) -> Option<String> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let ContentBlock::Text(text) = &chunk.content else {
                    return None;
                };
                let id = chunk
                    .message_id
                    .as_ref()
                    .map(std::string::ToString::to_string);
                let held = if self
                    .message
                    .as_ref()
                    .is_some_and(|(previous, _)| previous.as_deref() != id.as_deref())
                {
                    self.message.take().map(|(_, text)| text)
                } else {
                    None
                };
                let (_, accumulated) = self.message.get_or_insert_with(|| (id, String::new()));
                accumulated.push_str(&text.text);
                held.map(|text| format!("[作業報告] {text}"))
            }
            SessionUpdate::ToolCall(call) => {
                if let Some(name) = &call.name {
                    self.names
                        .insert(call.tool_call_id.to_string(), name.clone());
                }
                let name = call
                    .name
                    .clone()
                    .unwrap_or_else(|| call.tool_call_id.to_string());
                Some(format!("[作業報告] ツール {name}: {}", call.title))
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(name) = &update.fields.name {
                    self.names
                        .insert(update.tool_call_id.to_string(), name.clone());
                }
                let name = update
                    .fields
                    .name
                    .clone()
                    .or_else(|| self.names.get(&update.tool_call_id.to_string()).cloned())
                    .unwrap_or_else(|| update.tool_call_id.to_string());
                let status = update.fields.status.map_or("更新", |status| match status {
                    ToolCallStatus::Pending => "準備中",
                    ToolCallStatus::InProgress => "実行中",
                    ToolCallStatus::Completed => "完了",
                    ToolCallStatus::Failed => "失敗",
                    _ => "更新",
                });
                Some(format!("[作業報告] ツール {name}: {status}"))
            }
            SessionUpdate::Plan(plan) => {
                let entries = plan
                    .entries
                    .iter()
                    .map(|entry| {
                        let status = match entry.status {
                            PlanEntryStatus::Pending => "未着手",
                            PlanEntryStatus::InProgress => "進行中",
                            PlanEntryStatus::Completed => "完了",
                            _ => "保留",
                        };
                        format!("- {}（{status}）", entry.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!("[作業報告] 計画:\n{entries}"))
            }
            _ => None,
        }
    }
}

/// Canonical JSON object extracted from `ToolCallUpdate` fields for approval hashing and card display.
pub fn arguments_of(update: &ToolCallUpdate) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(title) = &update.fields.title {
        object.insert("title".into(), json!(title));
    }
    if let Some(name) = &update.fields.name {
        object.insert("name".into(), json!(name));
    }
    if let Some(content) = &update.fields.content {
        let text = content
            .iter()
            .filter_map(|item| match item {
                ene_plugin::ToolCallContent::Content(content) => match &content.content {
                    ContentBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        object.insert("content".into(), json!(text));
    }
    if let Some(locations) = &update.fields.locations {
        let paths = locations
            .iter()
            .map(|location| location.path.display().to_string())
            .collect::<Vec<_>>();
        object.insert("locations".into(), json!(paths));
    }
    if let Some(raw) = &update.fields.raw_input {
        object.insert("raw_input".into(), raw.clone());
    }
    if let Some(raw) = &update.fields.raw_output {
        object.insert("raw_output".into(), raw.clone());
    }
    Value::Object(object)
}

/// Selects the harness permission option matching an approval decision (`allow` vs deny).
pub fn select_permission_outcome(
    options: &[PermissionOption],
    allow: bool,
) -> RequestPermissionOutcome {
    let wanted = if allow { "allow" } else { "deny" };
    let kind = if allow {
        PermissionOptionKind::AllowOnce
    } else {
        PermissionOptionKind::RejectOnce
    };
    let selected = options
        .iter()
        .find(|option| option.option_id.to_string() == wanted)
        .or_else(|| options.iter().find(|option| option.kind == kind));
    selected.map_or(RequestPermissionOutcome::Cancelled, |option| {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option.option_id.clone()))
    })
}

/// Translates a harness [`StopReason`] and trailing message into `(RunState, report)`.
pub fn stop_outcome(stop_reason: StopReason, text: Option<String>) -> (RunState, Option<String>) {
    match stop_reason {
        StopReason::EndTurn => (RunState::Completed, text),
        StopReason::Cancelled => (RunState::Cancelled, None),
        StopReason::Refusal => (RunState::Failed, Some("作業を拒否しました".to_owned())),
        _ => (
            RunState::Failed,
            Some("作業を続けられませんでした: ハーネスの上限に達しました".to_owned()),
        ),
    }
}

/// Converts a plain instruction string into a single ACP text content block.
pub fn prompt_of(instruction: &str) -> Vec<ContentBlock> {
    vec![text_block(instruction)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ene_plugin::{
        ContentChunk, Plan, PlanEntry, PlanEntryPriority, ToolCall, ToolCallContent,
        ToolCallLocation, ToolCallUpdateFields,
    };

    #[test]
    fn buffer_accumulates_chunks_flushes_on_non_chunk_and_remembers_tool_names() {
        let mut buf = HarnessBuffer::default();
        let mut c1 = ContentChunk::new(text_block("前半 "));
        c1.message_id = Some("m1".into());
        assert_eq!(
            buf.apply_update(&SessionUpdate::AgentMessageChunk(c1)),
            UpdateEffects::default()
        );

        let mut c2 = ContentChunk::new(text_block("後半"));
        c2.message_id = Some("m2".into());
        assert_eq!(
            buf.apply_update(&SessionUpdate::AgentMessageChunk(c2)),
            UpdateEffects {
                flushed: None,
                report: Some("[作業報告] 前半 ".into()),
            }
        );

        let call = ToolCall::new("tc-1", "Inspect").name("fs.read");
        assert_eq!(
            buf.apply_update(&SessionUpdate::ToolCall(call)),
            UpdateEffects {
                flushed: Some("後半".into()),
                report: Some("[作業報告] ツール fs.read: Inspect".into()),
            }
        );

        let perm_update = ToolCallUpdate::new("tc-1", ToolCallUpdateFields::default());
        assert_eq!(buf.resolve_tool_name(&perm_update), "fs.read");

        let plan = Plan::new(vec![PlanEntry::new(
            "調査",
            PlanEntryPriority::Medium,
            PlanEntryStatus::InProgress,
        )]);
        assert_eq!(
            buf.apply_update(&SessionUpdate::Plan(plan)),
            UpdateEffects {
                flushed: None,
                report: Some("[作業報告] 計画:\n- 調査（進行中）".into()),
            }
        );
    }

    #[test]
    fn arguments_permission_and_stop_outcome_map_deterministically() {
        let fields = ToolCallUpdateFields::new()
            .title("Write")
            .name("fs.write")
            .content(vec![ToolCallContent::from(text_block("hello"))])
            .locations(vec![ToolCallLocation::new("/tmp/out.txt")]);
        let args = arguments_of(&ToolCallUpdate::new("tc-2", fields));
        assert_eq!(args["title"], "Write");
        assert_eq!(args["name"], "fs.write");
        assert_eq!(args["content"], "hello");
        assert_eq!(args["locations"], json!(["/tmp/out.txt"]));

        let options = vec![
            PermissionOption::new("custom_allow", "Allow", PermissionOptionKind::AllowOnce),
            PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
        ];
        assert!(matches!(
            select_permission_outcome(&options, true),
            RequestPermissionOutcome::Selected(sel) if sel.option_id.0.as_ref() == "custom_allow"
        ));
        assert!(matches!(
            select_permission_outcome(&options, false),
            RequestPermissionOutcome::Selected(sel) if sel.option_id.0.as_ref() == "deny"
        ));
        assert!(matches!(
            select_permission_outcome(&[], true),
            RequestPermissionOutcome::Cancelled
        ));

        assert_eq!(
            stop_outcome(StopReason::EndTurn, Some("done".into())),
            (RunState::Completed, Some("done".into()))
        );
        assert_eq!(
            stop_outcome(StopReason::Cancelled, Some("ignored".into())),
            (RunState::Cancelled, None)
        );
        assert_eq!(stop_outcome(StopReason::Refusal, None).0, RunState::Failed);
        assert_eq!(
            stop_outcome(StopReason::MaxTokens, None).0,
            RunState::Failed
        );
    }
}
