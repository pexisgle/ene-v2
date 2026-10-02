//! Host API wire types and notification synchronization, shared by host and client.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Companion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Queued,
    Streaming,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub companion_id: String,
    pub role: Role,
    pub text: String,
    pub status: MessageStatus,
    pub created_at: String,
    pub sequence: u64,
    #[serde(default)]
    pub resources: Vec<Value>,
    pub in_reply_to: Option<String>,
    pub task_id: Option<String>,
    pub question_id: Option<String>,
    pub failure: Option<Failure>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Companion {
    pub id: String,
    pub name: String,
    pub body: Option<Value>,
    pub voice: Option<Value>,
    pub mind: String,
    pub model_assignments: ModelAssignments,
    pub availability: Value,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelAssignments {
    pub companion: Option<Value>,
    pub task: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub revision: u64,
    pub connections: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub host_id: String,
    pub environment_id: String,
    pub capabilities: Value,
    pub cursor: Cursor,
    pub companions: Vec<Companion>,
    pub messages: Vec<Message>,
    pub message_cursors: BTreeMap<String, Option<String>>,
    pub tasks: Vec<Value>,
    pub questions: Vec<Value>,
    pub settings: Settings,
    pub grants: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub epoch: String,
    pub seq: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub companion_id: Option<String>,
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}

pub use crate::sync::{Replica, ResyncReason};
