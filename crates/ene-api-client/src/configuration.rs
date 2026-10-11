//! Stable identity and explicit progress for the multi-endpoint configuration workflow.
use super::*;

pub enum ConfigurationTarget {
    Existing(String),
    Create(String),
}
impl ConfigurationTarget {
    fn id(&self) -> &str {
        match self {
            Self::Existing(id) | Self::Create(id) => id,
        }
    }
}
pub enum CredentialChange {
    Secret(String),
    Environment,
    Revoke,
}
pub struct ConfigurationIntent {
    pub settings_revision: u64,
    pub companion_revision: u64,
    pub target: ConfigurationTarget,
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub options: JsonDocument,
    pub update_connection: bool,
    pub credential: Option<CredentialChange>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigurationProgress {
    Unconfirmed { target: String },
    ConnectionConfirmed { connection: String },
    AssignmentConfirmed { connection: String },
}
impl ConfigurationProgress {
    pub fn confirmed_connection(&self) -> Option<&str> {
        match self {
            Self::Unconfirmed { .. } => None,
            Self::ConnectionConfirmed { connection } | Self::AssignmentConfirmed { connection } => {
                Some(connection)
            }
        }
    }
}
pub struct ConfigurationFailure {
    pub progress: ConfigurationProgress,
    pub error: Box<Error>,
}
pub struct ConfigurationComplete {
    pub connection: String,
    pub credential_state: Option<String>,
}

impl ApiClient {
    pub async fn configure(
        &self,
        snapshot: &Snapshot,
        companion: &str,
        intent: ConfigurationIntent,
    ) -> Result<ConfigurationComplete, ConfigurationFailure> {
        let target = intent.target.id().to_owned();
        let mut progress = ConfigurationProgress::Unconfirmed {
            target: target.clone(),
        };
        let result = async {
            if target.is_empty() {
                return Err(Error::InvalidConnection);
            }
            let companion = snapshot
                .companions
                .iter()
                .find(|value| value.id == companion)
                .ok_or(Error::Protocol)?;
            if intent.update_connection {
                let providers = self.providers().await?;
                let provider = providers
                    .iter()
                    .find(|provider| provider.id == intent.provider && provider.state == "ready")
                    .ok_or(Error::Protocol)?;
                let defaults: serde_json::Value = if provider.options.json.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&provider.options.json).map_err(|_| Error::Protocol)?
                };
                let overrides: serde_json::Value =
                    serde_json::from_str(&intent.options.json).map_err(|_| Error::Protocol)?;
                let mut options = defaults.as_object().cloned().unwrap_or_default();
                options.extend(overrides.as_object().cloned().ok_or(Error::Protocol)?);
                let options = JsonDocument {
                    json: serde_json::Value::Object(options).to_string(),
                    ..Default::default()
                };
                let mut connections: Vec<_> = snapshot
                    .settings
                    .connections
                    .iter()
                    .map(|connection| ConnectionInput {
                        id: Some(connection.id.clone()),
                        create: false,
                        kind: connection.kind.clone(),
                        adapter: connection.adapter.clone(),
                        endpoint: connection.endpoint.clone(),
                        options: connection.options.clone(),
                        ..Default::default()
                    })
                    .collect();
                if let Some(connection) = connections
                    .iter_mut()
                    .find(|connection| connection.id.as_deref() == Some(&target))
                {
                    connection.adapter = intent.provider;
                    connection.endpoint = intent.endpoint;
                    connection.options = options.into();
                } else if matches!(intent.target, ConfigurationTarget::Create(_)) {
                    connections.push(ConnectionInput {
                        id: Some(target.clone()),
                        create: true,
                        kind: "llm".into(),
                        adapter: intent.provider,
                        endpoint: intent.endpoint,
                        options: options.into(),
                        ..Default::default()
                    });
                } else {
                    return Err(Error::Protocol);
                }
                let saved = self
                    .patch_settings(&UpdateSettingsRequest {
                        revision: intent.settings_revision,
                        connections,
                        ..Default::default()
                    })
                    .await?;
                if !saved
                    .connections
                    .iter()
                    .any(|connection| connection.id == target)
                {
                    return Err(Error::Protocol);
                }
            } else if !snapshot
                .settings
                .connections
                .iter()
                .any(|connection| connection.id == target)
            {
                return Err(Error::Protocol);
            }
            progress = ConfigurationProgress::ConnectionConfirmed {
                connection: target.clone(),
            };
            if !intent.model.trim().is_empty() {
                if companion
                    .model_assignments
                    .companion
                    .as_option()
                    .is_none_or(|assignment| {
                        assignment.connection_id != target || assignment.model_id != intent.model
                    })
                {
                    self.patch_companion(
                        &companion.id,
                        &UpdateCompanionRequest {
                            revision: intent.companion_revision,
                            companion_assignment: AssignmentPatch {
                                change: Some(types::assignment_patch::Change::Set(Box::new(
                                    ModelAssignment {
                                        connection_id: target.clone(),
                                        model_id: intent.model,
                                        ..Default::default()
                                    },
                                ))),
                                ..Default::default()
                            }
                            .into(),
                            ..Default::default()
                        },
                    )
                    .await?;
                }
                progress = ConfigurationProgress::AssignmentConfirmed {
                    connection: target.clone(),
                };
            }
            let credential_state = match intent.credential {
                Some(CredentialChange::Secret(secret)) => Some(
                    self.register_credentials(
                        &target,
                        &CredentialRequest {
                            secret: Some(secret),
                            ..Default::default()
                        },
                    )
                    .await?
                    .credential_state,
                ),
                Some(CredentialChange::Environment) => Some(
                    self.register_credentials(
                        &target,
                        &CredentialRequest {
                            source: Some("environment".into()),
                            ..Default::default()
                        },
                    )
                    .await?
                    .credential_state,
                ),
                Some(CredentialChange::Revoke) => {
                    self.revoke_credentials(&target).await?;
                    Some("missing".into())
                }
                None => None,
            };
            Ok(ConfigurationComplete {
                connection: target,
                credential_state,
            })
        }
        .await;
        result.map_err(|error| ConfigurationFailure {
            progress,
            error: Box::new(error),
        })
    }

    /// Confirmation reuses the exact receipt identity and never replays on a new host.
    pub async fn confirm_check(
        &self,
        host: &str,
        connection: &str,
        input: &CheckRequest,
        key: &IdempotencyKey,
    ) -> Result<CheckResult, Error> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let result = tokio::time::timeout_at(deadline, self.check(connection, input, key))
                .await
                .map_err(|_| Error::Transport)?;
            match result {
                Err(Error::Transport) => {}
                Err(Error::Api(error)) if error.body.error.code == "request_in_progress" => {}
                result => return result,
            }
            let snapshot = tokio::time::timeout_at(deadline, self.snapshot(None))
                .await
                .map_err(|_| Error::Transport)??;
            if snapshot.host_id != host {
                return Err(Error::ResyncRequired);
            }
            tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_millis(200)))
                .await
                .map_err(|_| Error::Transport)?;
        }
    }
}
