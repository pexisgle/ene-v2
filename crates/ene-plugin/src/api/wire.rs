//! Shared wire enums for operations, events and service requests.
use super::{
    CharacterAction, CharacterEvent, CharacterTarget, GenerationRequest, HarnessEvent,
    HarnessInput, HttpRequest,
};
use crate::manifest::{Extension, Service};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ListModels {
        endpoint: String,
        options: Value,
    },
    Generate {
        request: GenerationRequest,
    },
    Character {
        target: CharacterTarget,
        event: CharacterEvent,
    },
    Harness {
        input: HarnessInput,
    },
}

impl Operation {
    pub fn extension(&self) -> Extension {
        match self {
            Self::ListModels { .. } | Self::Generate { .. } => Extension::Llm,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
        }
    }
}

// Built once per plugin event and consumed immediately; boxing the harness
// variant would add an allocation to every update.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Delta {
        text: String,
    },
    Models {
        models: Vec<crate::provider::Model>,
    },
    Finished {
        reason: String,
    },
    Character {
        target: CharacterTarget,
        action: CharacterAction,
    },
    Harness {
        event: HarnessEvent,
    },
}

impl Event {
    pub fn extension(&self) -> Extension {
        match self {
            Self::Delta { .. } | Self::Models { .. } | Self::Finished { .. } => Extension::Llm,
            Self::Character { .. } => Extension::Character,
            Self::Harness { .. } => Extension::Harness,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceRequest {
    Log { message: String },
    Configuration,
    StateRead { key: String },
    StateWrite { key: String, value: Value },
    HttpOpen { request: HttpRequest },
    HttpRead { handle: String },
    HttpClose { handle: String },
    Models { request: GenerationRequest },
    Tool { name: String, arguments: Value },
}

impl ServiceRequest {
    pub fn service(&self) -> Service {
        match self {
            Self::Log { .. } => Service::Log,
            Self::Configuration => Service::Configuration,
            Self::StateRead { .. } | Self::StateWrite { .. } => Service::State,
            Self::HttpOpen { .. } | Self::HttpRead { .. } | Self::HttpClose { .. } => Service::Http,
            Self::Models { .. } => Service::Models,
            Self::Tool { .. } => Service::Tools,
        }
    }
}
