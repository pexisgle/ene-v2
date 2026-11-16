use ene_plugin::{Event, extism_pdk};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const MAX_CHOICES: usize = 32;
const MAX_CALLS: usize = 64;
const MAX_METADATA: usize = 256;
const MAX_FINISH_REASON: usize = 128;
const MAX_RETAINED: usize = 8 * 1024 * 1024;
pub(super) const MAX_EVENT: usize = 128 * 1024;

#[derive(Ord, PartialOrd, Eq, PartialEq)]
enum CallKey {
    Indexed(u64),
    Complete(usize),
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct PendingChoice {
    reason: Option<String>,
    calls: BTreeMap<CallKey, PendingCall>,
}

#[derive(Default)]
pub(super) struct PendingCalls {
    choices: BTreeMap<u64, PendingChoice>,
    call_count: usize,
    retained: usize,
    reason: Option<String>,
}

fn failure(message: &str) -> extism_pdk::Error {
    extism_pdk::Error::msg(message.to_owned())
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, extism_pdk::Error> {
    value
        .get(key)
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| failure("invalid tool call field"))
        })
        .transpose()
}

impl PendingCalls {
    pub(super) fn push(&mut self, choices: &[Value]) -> Result<(), extism_pdk::Error> {
        if choices.len() > MAX_CHOICES {
            return Err(failure("choice limit"));
        }
        let text_only = self.call_count == 0
            && choices
                .iter()
                .all(|value| value["delta"].get("tool_calls").is_none());
        let mut seen = BTreeSet::new();
        for (position, value) in choices.iter().enumerate() {
            let index = match value.get("index") {
                Some(index) => index
                    .as_u64()
                    .ok_or_else(|| failure("invalid choice index"))?,
                None if text_only => position as u64,
                None if choices.len() == 1 && self.choices.keys().all(|index| *index == 0) => 0,
                None => return Err(failure("ambiguous choice index")),
            };
            if !seen.insert(index) {
                return Err(failure("duplicate choice index"));
            }
            if !self.choices.contains_key(&index) && self.choices.len() == MAX_CHOICES {
                return Err(failure("choice limit"));
            }
            let choice = self.choices.entry(index).or_default();
            if let Some(reason) = value
                .get("finish_reason")
                .filter(|reason| !reason.is_null())
            {
                let reason = reason
                    .as_str()
                    .ok_or_else(|| failure("invalid finish reason"))?;
                if reason.is_empty() || reason.len() > MAX_FINISH_REASON {
                    return Err(failure("finish reason limit"));
                }
                if !text_only
                    && choice
                        .reason
                        .as_deref()
                        .is_some_and(|previous| previous != reason)
                {
                    return Err(failure("conflicting finish reason"));
                }
                self.retained -= choice.reason.as_ref().map_or(0, String::len);
                self.retained += reason.len();
                choice.reason = Some(reason.into());
                self.reason = Some(reason.into());
                if self.retained > MAX_RETAINED {
                    return Err(failure("pending tool bytes limit"));
                }
            }
            let Some(calls) = value.get("delta").and_then(|delta| delta.get("tool_calls")) else {
                continue;
            };
            let calls = calls
                .as_array()
                .ok_or_else(|| failure("invalid tool calls"))?;
            if calls.len() > MAX_CALLS {
                return Err(failure("tool call limit"));
            }
            let mut seen = BTreeSet::new();
            for value in calls {
                if !value.is_object() {
                    return Err(failure("invalid tool call"));
                }
                if let Some(kind) = value.get("type")
                    && kind.as_str() != Some("function")
                {
                    return Err(failure("invalid tool call type"));
                }
                let id = string_field(value, "id")?;
                let function = value.get("function");
                if function.is_some_and(|function| !function.is_object()) {
                    return Err(failure("invalid tool function"));
                }
                let name = function
                    .map(|function| string_field(function, "name"))
                    .transpose()?
                    .flatten();
                let arguments = function
                    .map(|function| string_field(function, "arguments"))
                    .transpose()?
                    .flatten();
                let key = match value.get("index") {
                    Some(index) => CallKey::Indexed(
                        index
                            .as_u64()
                            .ok_or_else(|| failure("invalid tool index"))?,
                    ),
                    None => {
                        let id = id
                            .filter(|id| !id.trim().is_empty())
                            .ok_or_else(|| failure("ambiguous unindexed tool call"))?;
                        if name.is_none_or(|name| name.trim().is_empty()) {
                            return Err(failure("incomplete unindexed tool call"));
                        }
                        let arguments: Value = serde_json::from_str(
                            arguments.ok_or_else(|| failure("incomplete unindexed tool call"))?,
                        )?;
                        if !arguments.is_object() {
                            return Err(failure("tool arguments must be an object"));
                        }
                        if choice
                            .calls
                            .iter()
                            .any(|(key, call)| matches!(key, CallKey::Complete(_)) && call.id == id)
                        {
                            return Err(failure("duplicate unindexed tool call"));
                        }
                        CallKey::Complete(self.call_count)
                    }
                };
                if let CallKey::Indexed(index) = &key
                    && !seen.insert(*index)
                {
                    return Err(failure("duplicate tool index"));
                }
                if !choice.calls.contains_key(&key) {
                    if self.call_count == MAX_CALLS {
                        return Err(failure("tool call limit"));
                    }
                    self.call_count += 1;
                }
                let call = choice.calls.entry(key).or_default();
                if let Some(id) = id.filter(|id| !id.is_empty()) {
                    if id.len() > MAX_METADATA {
                        return Err(failure("tool metadata limit"));
                    }
                    if !call.id.is_empty() && call.id != id {
                        return Err(failure("conflicting tool id"));
                    }
                    if call.id.is_empty() {
                        self.retained += id.len();
                        call.id.push_str(id);
                    }
                }
                if let Some(name) = name {
                    if call.name.len().saturating_add(name.len()) > MAX_METADATA {
                        return Err(failure("tool metadata limit"));
                    }
                    self.retained += name.len();
                    call.name.push_str(name);
                }
                if let Some(arguments) = arguments {
                    self.retained += arguments.len();
                    call.arguments.push_str(arguments);
                }
                if self.retained > MAX_RETAINED {
                    return Err(failure("pending tool bytes limit"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn has_calls(&self) -> bool {
        self.call_count != 0
    }

    pub(super) fn finish(self, terminal: bool) -> Result<(Vec<Event>, String), extism_pdk::Error> {
        let reason = self.reason.unwrap_or_else(|| "incomplete".into());
        if reason.is_empty() || reason.len() > MAX_FINISH_REASON {
            return Err(failure("finish reason limit"));
        }
        if self.call_count > MAX_CALLS {
            return Err(failure("tool call limit"));
        }
        let mut events = Vec::with_capacity(self.call_count);
        let mut ids = BTreeSet::new();
        for choice in self.choices.into_values() {
            if !terminal && !choice.calls.is_empty() && choice.reason.is_none() {
                return Err(failure("incomplete tool response without finish reason"));
            }
            for call in choice.calls.into_values() {
                if call.id.trim().is_empty() || call.name.trim().is_empty() {
                    return Err(failure("incomplete tool call"));
                }
                if call.id.len() > MAX_METADATA || call.name.len() > MAX_METADATA {
                    return Err(failure("tool metadata limit"));
                }
                if !ids.insert(call.id.clone()) {
                    return Err(failure("duplicate tool id"));
                }
                let arguments: Value = serde_json::from_str(&call.arguments)?;
                if !arguments.is_object() {
                    return Err(failure("tool arguments must be an object"));
                }
                let event = Event::ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments,
                };
                if serde_json::to_vec(&event)?.len() > MAX_EVENT {
                    return Err(failure("tool event limit"));
                }
                events.push(event);
            }
        }
        Ok((events, reason))
    }
}
