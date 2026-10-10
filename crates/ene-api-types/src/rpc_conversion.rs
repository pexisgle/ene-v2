//! Explicit conversion at the preserved storage/plugin boundary.
//! RPC DTOs are generated; these functions preserve legacy JSON and open strings.
use crate::{host as h, provider as p, rpc as w};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

pub type ConversionError = serde_json::Error;
pub trait RpcConvert: Sized {
    type Wire;
    fn into_wire(self) -> Result<Self::Wire, ConversionError>;
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError>;
}
pub fn invalid() -> ConversionError {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid RPC value",
    ))
}
fn enum_string(value: impl Serialize) -> Result<String, ConversionError> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(invalid)
}
fn enum_value<T: DeserializeOwned>(value: String) -> Result<T, ConversionError> {
    serde_json::from_value(Value::String(value))
}
impl RpcConvert for Value {
    type Wire = w::JsonDocument;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::JsonDocument {
            json: serde_json::to_string(&self)?,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        serde_json::from_str(&value.json)
    }
}

impl RpcConvert for h::Cursor {
    type Wire = w::Cursor;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Cursor {
            epoch: self.epoch,
            seq: self.seq,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            epoch: value.epoch,
            seq: value.seq,
        })
    }
}

impl RpcConvert for h::Failure {
    type Wire = w::Failure;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Failure {
            code: self.code,
            message: self.message,
            details: self.details.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            code: value.code,
            message: value.message,
            details: RpcConvert::from_wire(value.details.into_option().ok_or_else(invalid)?)?,
        })
    }
}

impl RpcConvert for h::Message {
    type Wire = w::Message;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Message {
            id: self.id,
            companion_id: self.companion_id,
            role: enum_string(self.role)?,
            text: self.text,
            status: enum_string(self.status)?,
            created_at: self.created_at,
            sequence: self.sequence,
            resources: self
                .resources
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            in_reply_to: self.in_reply_to,
            task_id: self.task_id,
            question_id: self.question_id,
            failure: self.failure.map(RpcConvert::into_wire).transpose()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            companion_id: value.companion_id,
            role: enum_value(value.role)?,
            text: value.text,
            status: enum_value(value.status)?,
            created_at: value.created_at,
            sequence: value.sequence,
            resources: value
                .resources
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            in_reply_to: value.in_reply_to,
            task_id: value.task_id,
            question_id: value.question_id,
            failure: value
                .failure
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
        })
    }
}

impl RpcConvert for h::Companion {
    type Wire = w::Companion;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Companion {
            id: self.id,
            name: self.name,
            body: self.body.map(RpcConvert::into_wire).transpose()?.into(),
            voice: self.voice.map(RpcConvert::into_wire).transpose()?.into(),
            mind: self.mind,
            model_assignments: self.model_assignments.into_wire()?.into(),
            availability: self.availability.into_wire()?.into(),
            revision: self.revision,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            name: value.name,
            body: value
                .body
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
            voice: value
                .voice
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
            mind: value.mind,
            model_assignments: RpcConvert::from_wire(
                value.model_assignments.into_option().ok_or_else(invalid)?,
            )?,
            availability: RpcConvert::from_wire(
                value.availability.into_option().ok_or_else(invalid)?,
            )?,
            revision: value.revision,
        })
    }
}

impl RpcConvert for h::ModelAssignments {
    type Wire = w::ModelAssignments;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::ModelAssignments {
            companion: self
                .companion
                .map(RpcConvert::into_wire)
                .transpose()?
                .into(),
            task: self.task.map(RpcConvert::into_wire).transpose()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            companion: value
                .companion
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
            task: value
                .task
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
        })
    }
}

impl RpcConvert for h::Settings {
    type Wire = w::Settings;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Settings {
            revision: self.revision,
            connections: self
                .connections
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            revision: value.revision,
            connections: value
                .connections
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl RpcConvert for h::Snapshot {
    type Wire = w::Snapshot;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        if !self.tasks.is_empty() || !self.questions.is_empty() {
            return Err(invalid());
        }
        Ok(w::Snapshot {
            host_id: self.host_id,
            environment_id: self.environment_id,
            capabilities: self.capabilities.into_wire()?.into(),
            cursor: self.cursor.into_wire()?.into(),
            companions: self
                .companions
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            messages: self
                .messages
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            message_cursors: self
                .message_cursors
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        w::HistoryCursor {
                            value: v,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            settings: self.settings.into_wire()?.into(),
            grants: self
                .grants
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            health: self.health.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            host_id: value.host_id,
            environment_id: value.environment_id,
            capabilities: RpcConvert::from_wire(
                value.capabilities.into_option().ok_or_else(invalid)?,
            )?,
            cursor: RpcConvert::from_wire(value.cursor.into_option().ok_or_else(invalid)?)?,
            companions: value
                .companions
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            messages: value
                .messages
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            message_cursors: value
                .message_cursors
                .into_iter()
                .map(|(k, v)| (k, v.value))
                .collect(),
            tasks: Vec::new(),
            questions: Vec::new(),
            settings: RpcConvert::from_wire(value.settings.into_option().ok_or_else(invalid)?)?,
            grants: value
                .grants
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            health: RpcConvert::from_wire(value.health.into_option().ok_or_else(invalid)?)?,
        })
    }
}

impl RpcConvert for h::HostHealth {
    type Wire = w::HostHealth;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::HostHealth {
            ready: self.ready,
            recovery_required: self.recovery_required,
            stopping: self.stopping,
            credential_cleanup_pending: self.credential_cleanup_pending as u64,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            ready: value.ready,
            recovery_required: value.recovery_required,
            stopping: value.stopping,
            credential_cleanup_pending: usize::try_from(value.credential_cleanup_pending)
                .map_err(|_| invalid())?,
        })
    }
}

impl RpcConvert for h::OperationAvailability {
    type Wire = w::OperationAvailability;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::OperationAvailability {
            operation: self.operation,
            purpose: self.purpose,
            data_kinds: self.data_kinds.into_iter().collect(),
            authorization: enum_string(self.authorization)?,
            dependency: enum_string(self.dependency)?,
            busy: self.busy,
            runnable: self.runnable,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            operation: value.operation,
            purpose: value.purpose,
            data_kinds: value.data_kinds.into_iter().collect(),
            authorization: enum_value(value.authorization)?,
            dependency: enum_value(value.dependency)?,
            busy: value.busy,
            runnable: value.runnable,
        })
    }
}

impl RpcConvert for h::ConnectionAvailability {
    type Wire = w::Availability;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Availability {
            companion_id: self.companion_id,
            connection_id: self.connection_id,
            settings_revision: self.settings_revision,
            provider_revision: self.provider_revision,
            destinations: self.destinations.into_iter().collect(),
            connection_fingerprint: self.connection_fingerprint.into_wire()?.into(),
            operations: self
                .operations
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            companion_id: value.companion_id,
            connection_id: value.connection_id,
            settings_revision: value.settings_revision,
            provider_revision: value.provider_revision,
            destinations: value.destinations.into_iter().collect(),
            connection_fingerprint: RpcConvert::from_wire(
                value
                    .connection_fingerprint
                    .into_option()
                    .ok_or_else(invalid)?,
            )?,
            operations: value
                .operations
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl RpcConvert for h::Connection {
    type Wire = w::Connection;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Connection {
            id: self.id,
            kind: self.kind,
            adapter: self.adapter,
            endpoint: self.endpoint,
            options: self.options.into_wire()?.into(),
            credential_state: self.credential_state,
            capabilities: self
                .capabilities
                .map(RpcConvert::into_wire)
                .transpose()?
                .into(),
            last_check: self
                .last_check
                .map(RpcConvert::into_wire)
                .transpose()?
                .into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            kind: value.kind,
            adapter: value.adapter,
            endpoint: value.endpoint,
            options: RpcConvert::from_wire(value.options.into_option().ok_or_else(invalid)?)?,
            credential_state: value.credential_state,
            capabilities: value
                .capabilities
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
            last_check: value
                .last_check
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
        })
    }
}

impl RpcConvert for h::DataTransferGrant {
    type Wire = w::Grant;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Grant {
            id: self.id,
            companion_id: self.companion_id,
            kind: self.kind,
            connection_id: self.connection_id,
            destinations: self.destinations.into_iter().collect(),
            data_kinds: self.data_kinds.into_iter().collect(),
            purpose: self.purpose,
            status: self.status,
            revision: self.revision,
            invalidation_reason: self.invalidation_reason,
            connection_fingerprint: self.connection_fingerprint.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            companion_id: value.companion_id,
            kind: value.kind,
            connection_id: value.connection_id,
            destinations: value.destinations.into_iter().collect(),
            data_kinds: value.data_kinds.into_iter().collect(),
            purpose: value.purpose,
            status: value.status,
            revision: value.revision,
            invalidation_reason: value.invalidation_reason,
            connection_fingerprint: RpcConvert::from_wire(
                value
                    .connection_fingerprint
                    .into_option()
                    .ok_or_else(invalid)?,
            )?,
        })
    }
}

impl RpcConvert for h::MessageAcceptance {
    type Wire = w::MessageAcceptance;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::MessageAcceptance {
            request_id: self.request_id,
            message_id: self.message_id,
            response_message_id: self.response_message_id,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            request_id: value.request_id,
            message_id: value.message_id,
            response_message_id: value.response_message_id,
        })
    }
}

impl RpcConvert for h::ModelAssignment {
    type Wire = w::ModelAssignment;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::ModelAssignment {
            connection_id: self.connection_id,
            model_id: self.model_id,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            connection_id: value.connection_id,
            model_id: value.model_id,
        })
    }
}

impl RpcConvert for h::CredentialResult {
    type Wire = w::CredentialResult;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::CredentialResult {
            credential_state: self.credential_state,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            credential_state: value.credential_state,
        })
    }
}

impl RpcConvert for h::CheckResult {
    type Wire = w::CheckResult;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::CheckResult {
            request_id: self.request_id,
            capabilities: self.capabilities.into_wire()?.into(),
            success: self.success,
            model_id: self.model_id,
            content_chunks: self.content_chunks,
            unicode_scalars: self.unicode_scalars,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            request_id: value.request_id,
            capabilities: RpcConvert::from_wire(
                value.capabilities.into_option().ok_or_else(invalid)?,
            )?,
            success: value.success,
            model_id: value.model_id,
            content_chunks: value.content_chunks,
            unicode_scalars: value.unicode_scalars,
        })
    }
}

impl RpcConvert for h::GrantResult {
    type Wire = w::GrantResult;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        if !self.questions.is_empty() {
            return Err(invalid());
        }
        Ok(w::GrantResult {
            grant: self.grant.into_wire()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            grant: RpcConvert::from_wire(value.grant.into_option().ok_or_else(invalid)?)?,
            questions: Vec::new(),
        })
    }
}

impl RpcConvert for h::MessageDelta {
    type Wire = w::MessageDelta;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::MessageDelta {
            message_id: self.message_id,
            offset: self.offset,
            delta: self.delta,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            message_id: value.message_id,
            offset: value.offset,
            delta: value.delta,
        })
    }
}

impl RpcConvert for p::SettingField {
    type Wire = w::SettingField;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::SettingField {
            key: self.key,
            label: self.label,
            kind: enum_string(self.kind)?,
            required: self.required,
            choices: self.choices.into_iter().collect(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            key: value.key,
            label: value.label,
            kind: enum_value(value.kind)?,
            required: value.required,
            choices: value.choices.into_iter().collect(),
        })
    }
}

impl RpcConvert for p::Model {
    type Wire = w::Model;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Model {
            id: self.id,
            name: self.name,
            metadata: self.metadata.map(RpcConvert::into_wire).transpose()?.into(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            name: value.name,
            metadata: value
                .metadata
                .into_option()
                .map(RpcConvert::from_wire)
                .transpose()?,
        })
    }
}

impl RpcConvert for p::ModelMetadata {
    type Wire = w::ModelMetadata;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::ModelMetadata {
            source: self.source,
            scope: enum_string(self.scope)?,
            context_window: self.context_window,
            max_input_tokens: self.max_input_tokens,
            max_output_tokens: self.max_output_tokens,
            input_modalities: self
                .input_modalities
                .map(|values| w::StringList {
                    values,
                    ..Default::default()
                })
                .into(),
            output_modalities: self
                .output_modalities
                .map(|values| w::StringList {
                    values,
                    ..Default::default()
                })
                .into(),
            supported_parameters: self
                .supported_parameters
                .map(|values| w::StringList {
                    values,
                    ..Default::default()
                })
                .into(),
            field_sources: self.field_sources.into_iter().collect(),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            source: value.source,
            scope: enum_value(value.scope)?,
            context_window: value.context_window,
            max_input_tokens: value.max_input_tokens,
            max_output_tokens: value.max_output_tokens,
            input_modalities: value.input_modalities.into_option().map(|v| v.values),
            output_modalities: value.output_modalities.into_option().map(|v| v.values),
            supported_parameters: value.supported_parameters.into_option().map(|v| v.values),
            field_sources: value.field_sources.into_iter().collect(),
        })
    }
}

impl RpcConvert for p::Capabilities {
    type Wire = w::ProviderCapabilities;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::ProviderCapabilities {
            streaming_text: self.streaming_text,
            model_list: self.model_list,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            streaming_text: value.streaming_text,
            model_list: value.model_list,
        })
    }
}

impl RpcConvert for p::ProviderInfo {
    type Wire = w::Provider;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        Ok(w::Provider {
            id: self.id,
            name: self.name,
            kind: self.kind,
            endpoint: self.endpoint,
            options: self.options.into_wire()?.into(),
            settings: self
                .settings
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            requires_credentials: self.requires_credentials,
            environment_variable: self.environment_variable,
            destinations: self.destinations.into_iter().collect(),
            state: self.state,
            error: self.error,
            revision: self.revision,
            capabilities: self.capabilities.into_wire()?.into(),
            models: self
                .models
                .into_iter()
                .map(RpcConvert::into_wire)
                .collect::<Result<_, _>>()?,
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        Ok(Self {
            id: value.id,
            name: value.name,
            kind: value.kind,
            endpoint: value.endpoint,
            options: RpcConvert::from_wire(value.options.into_option().ok_or_else(invalid)?)?,
            settings: value
                .settings
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
            requires_credentials: value.requires_credentials,
            environment_variable: value.environment_variable,
            destinations: value.destinations.into_iter().collect(),
            state: value.state,
            error: value.error,
            revision: value.revision,
            capabilities: RpcConvert::from_wire(
                value.capabilities.into_option().ok_or_else(invalid)?,
            )?,
            models: value
                .models
                .into_iter()
                .map(RpcConvert::from_wire)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl RpcConvert for h::Notification {
    type Wire = w::Notification;
    fn into_wire(self) -> Result<Self::Wire, ConversionError> {
        use w::notification::Event;
        fn payload<T: RpcConvert + DeserializeOwned>(v: Value) -> Result<T::Wire, ConversionError> {
            serde_json::from_value::<T>(v)?.into_wire()
        }
        let event = match self.kind.as_str() {
            "message.created" => Event::MessageCreated(Box::new(payload::<h::Message>(self.data)?)),
            "message.started" => Event::MessageStarted(Box::new(payload::<h::Message>(self.data)?)),
            "message.updated" => Event::MessageUpdated(Box::new(payload::<h::Message>(self.data)?)),
            "message.delta" => {
                Event::MessageDelta(Box::new(payload::<h::MessageDelta>(self.data)?))
            }
            "companion.updated" => {
                Event::CompanionUpdated(Box::new(payload::<h::Companion>(self.data)?))
            }
            "settings.updated" => {
                Event::SettingsUpdated(Box::new(payload::<h::Settings>(self.data)?))
            }
            "grants.updated" => Event::GrantsUpdated(Box::new(w::Grants {
                items: serde_json::from_value::<Vec<h::DataTransferGrant>>(self.data)?
                    .into_iter()
                    .map(RpcConvert::into_wire)
                    .collect::<Result<_, _>>()?,
                ..Default::default()
            })),
            "host.storage_failed" | "generation.persistence_failed" => {
                let failure = Box::new(w::PersistenceFailure {
                    message_id: self.data["message_id"].as_str().map(str::to_owned),
                    code: self.data["code"].as_str().ok_or_else(invalid)?.into(),
                    ..Default::default()
                });
                if self.kind == "host.storage_failed" {
                    Event::HostStorageFailed(failure)
                } else {
                    Event::GenerationPersistenceFailed(failure)
                }
            }
            _ => return Err(invalid()),
        };
        Ok(w::Notification {
            cursor: h::Cursor {
                epoch: self.epoch,
                seq: self.seq,
            }
            .into_wire()?
            .into(),
            companion_id: self.companion_id,
            event: Some(event),
            ..Default::default()
        })
    }
    fn from_wire(value: Self::Wire) -> Result<Self, ConversionError> {
        use w::notification::Event;
        fn payload<T: RpcConvert + Serialize>(v: T::Wire) -> Result<Value, ConversionError> {
            serde_json::to_value(T::from_wire(v)?)
        }
        let cursor = h::Cursor::from_wire(value.cursor.into_option().ok_or_else(invalid)?)?;
        let (kind, data) = match value.event.ok_or_else(invalid)? {
            Event::MessageCreated(v) => ("message.created", payload::<h::Message>(*v)?),
            Event::MessageStarted(v) => ("message.started", payload::<h::Message>(*v)?),
            Event::MessageUpdated(v) => ("message.updated", payload::<h::Message>(*v)?),
            Event::MessageDelta(v) => ("message.delta", payload::<h::MessageDelta>(*v)?),
            Event::CompanionUpdated(v) => ("companion.updated", payload::<h::Companion>(*v)?),
            Event::SettingsUpdated(v) => ("settings.updated", payload::<h::Settings>(*v)?),
            Event::GrantsUpdated(v) => (
                "grants.updated",
                serde_json::to_value(
                    v.items
                        .into_iter()
                        .map(h::DataTransferGrant::from_wire)
                        .collect::<Result<Vec<_>, _>>()?,
                )?,
            ),
            Event::HostStorageFailed(v) => {
                ("host.storage_failed", serde_json::json!({"code":v.code}))
            }
            Event::GenerationPersistenceFailed(v) => (
                "generation.persistence_failed",
                serde_json::json!({"code":v.code,"message_id":v.message_id}),
            ),
        };
        Ok(h::Notification {
            epoch: cursor.epoch,
            seq: cursor.seq,
            kind: kind.into(),
            companion_id: value.companion_id,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protobuf_json_and_binary_keep_u64_unicode_and_unknown_storage_values() {
        use buffa::Message as _;
        let source:h::Message=serde_json::from_value(serde_json::json!({"id":"m","companion_id":"c","role":"future_role","text":"a😀日本語","status":"future_status","created_at":"now","sequence":u64::MAX,"resources":[{"value":u64::MAX}],"in_reply_to":null,"task_id":null,"question_id":null,"failure":null})).unwrap();
        let wire = source.clone().into_wire().unwrap();
        let json = serde_json::to_value(&wire).unwrap();
        assert_eq!(json["sequence"], u64::MAX.to_string());
        let json_wire: w::Message = serde_json::from_value(json).unwrap();
        let binary = w::Message::decode_from_slice(&json_wire.encode_to_vec()).unwrap();
        let restored = h::Message::from_wire(binary).unwrap();
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            serde_json::to_value(restored).unwrap()
        );
        assert!(h::Notification::from_wire(w::Notification::default()).is_err());
    }
}
