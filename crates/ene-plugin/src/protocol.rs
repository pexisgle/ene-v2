//! Host and worker communication contracts.
use crate::api::{Event, Operation, ServiceRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub request_id: String,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
impl Failure {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.into(),
            message: code.into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Initialize {
        contract_version: u32,
        call: Call,
        entrypoint: String,
    },
    Invoke {
        call: Call,
        operation: Operation,
    },
    ServiceResult {
        call: Call,
        service_id: String,
        result: Result<Value, Failure>,
    },
    Cancel {
        call: Call,
    },
    Shutdown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerMessage {
    Ready {
        contract_version: u32,
        call: Call,
        capabilities: Value,
    },
    Event {
        call: Call,
        sequence: u64,
        event: Event,
    },
    Service {
        call: Call,
        service_id: String,
        request: ServiceRequest,
    },
    Complete {
        call: Call,
        result: Value,
    },
    Failed {
        call: Call,
        error: Failure,
    },
}
