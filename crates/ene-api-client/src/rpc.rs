//! Typed operations on the generated Connect clients.
use super::*;
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use ene_api_types::{rpc as w, rpc_conversion::RpcConvert};
pub type HostRpcClient = w::HostServiceClient<HttpClient>;
pub type PluginRpcClient = w::PluginServiceClient<HttpClient>;
pub type DeviceRpcClient = w::DeviceServiceClient<HttpClient>;
pub(crate) struct RpcApi {
    pub host: HostRpcClient,
    pub devices: DeviceRpcClient,
    pub plugins: PluginRpcClient,
}
impl RpcApi {
    pub fn new(endpoint: &Url, bearer: HeaderValue) -> Result<Self, Error> {
        let builder = HttpClient::builder().connect_timeout(Duration::from_secs(5));
        let transport = if endpoint.scheme() == "https" {
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            // Explicit provider remains deterministic when the workspace also uses aws-lc.
            let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::Transport)?
            .with_root_certificates(roots)
            .with_no_client_auth();
            builder.with_tls(Arc::new(tls))
        } else {
            builder.plaintext()
        };
        let config = ClientConfig::new(
            endpoint
                .as_str()
                .parse()
                .map_err(|_| Error::InvalidConnection)?,
        )
        .with_default_header(AUTHORIZATION, bearer)
        .with_default_max_message_size(MAX_RESPONSE);
        Ok(Self {
            host: HostRpcClient::new(transport.clone(), config.clone()),
            plugins: PluginRpcClient::new(transport.clone(), config.clone()),
            devices: DeviceRpcClient::new(transport, config),
        })
    }
}
impl ApiClient {
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<Snapshot, Error> {
        decode(
            self.0
                .rpc
                .host
                .get_snapshot_with_options(
                    w::SnapshotRequest {
                        subscription_id: subscription.map(Into::into),
                        ..Default::default()
                    },
                    options(5),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        self.0
            .rpc
            .host
            .list_companions_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        decode(
            self.0
                .rpc
                .host
                .get_companion_with_options(
                    w::Id {
                        id: id.into(),
                        ..Default::default()
                    },
                    options(5),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn patch_companion(
        &self,
        id: &str,
        input: &CompanionPatch,
    ) -> Result<Companion, Error> {
        let assignment = |slot: &str| -> Result<Option<w::AssignmentPatch>, Error> {
            let Some(value) = input.model_assignments.as_ref().and_then(|v| v.get(slot)) else {
                return Ok(None);
            };
            let change = match value {
                Some(value) => w::assignment_patch::Change::Set(Box::new(encode(value.clone())?)),
                None => w::assignment_patch::Change::Clear(Box::default()),
            };
            Ok(Some(w::AssignmentPatch {
                change: Some(change),
                ..Default::default()
            }))
        };
        if input
            .model_assignments
            .as_ref()
            .is_some_and(|values| values.keys().any(|s| s != "companion" && s != "task"))
        {
            return Err(Error::Protocol);
        }
        decode(
            self.0
                .rpc
                .host
                .update_companion_with_options(
                    w::UpdateCompanionRequest {
                        id: id.into(),
                        revision: input.revision,
                        name: input.name.clone(),
                        mind: input.mind.clone(),
                        companion_assignment: assignment("companion")?.into(),
                        task_assignment: assignment("task")?.into(),
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn messages(
        &self,
        companion: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Page<Message>, Error> {
        let response = self
            .0
            .rpc
            .host
            .get_history_with_options(
                w::HistoryRequest {
                    companion_id: companion.into(),
                    cursor: cursor.map(Into::into),
                    limit: limit
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| Error::Protocol)?,
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?
            .into_owned();
        Ok(Page {
            items: response
                .items
                .into_iter()
                .map(decode)
                .collect::<Result<_, _>>()?,
            next_cursor: response.next_cursor,
        })
    }
    pub async fn message(&self, companion: &str, id: &str) -> Result<Message, Error> {
        decode(
            self.0
                .rpc
                .host
                .get_message_with_options(
                    w::MessageRequest {
                        companion_id: companion.into(),
                        message_id: id.into(),
                        ..Default::default()
                    },
                    options(5),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn send_message(
        &self,
        companion: &str,
        input: &MessageInput,
        key: &IdempotencyKey,
    ) -> Result<MessageAcceptance, Error> {
        decode(
            self.0
                .rpc
                .host
                .send_message_with_options(
                    w::SendMessageRequest {
                        companion_id: companion.into(),
                        text: input.text.clone(),
                        request_key: key.0.clone(),
                        resources: input
                            .resources
                            .iter()
                            .cloned()
                            .map(encode)
                            .collect::<Result<_, _>>()?,
                        question_id: input.question_id.clone(),
                        decision: input.decision.clone(),
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        decode(
            self.0
                .rpc
                .host
                .get_settings_with_options(w::Empty::default(), options(5))
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn patch_settings(&self, input: &SettingsPatch) -> Result<Settings, Error> {
        let connections = input
            .connections
            .iter()
            .map(|c| {
                Ok(w::ConnectionInput {
                    id: c.id.clone(),
                    create: c.create,
                    kind: c.kind.clone(),
                    adapter: c.adapter.clone(),
                    endpoint: c.endpoint.clone(),
                    options: encode(c.options.clone())?.into(),
                    ..Default::default()
                })
            })
            .collect::<Result<_, Error>>()?;
        decode(
            self.0
                .rpc
                .host
                .update_settings_with_options(
                    w::UpdateSettingsRequest {
                        revision: input.revision,
                        connections,
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<DataTransferGrant>, Error> {
        self.0
            .rpc
            .host
            .list_grants_with_options(
                w::GrantsRequest {
                    companion_id: companion.map(Into::into),
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn create_grant(
        &self,
        input: &GrantInput,
        key: &IdempotencyKey,
    ) -> Result<GrantResult, Error> {
        decode(
            self.0
                .rpc
                .host
                .create_grant_with_options(
                    w::CreateGrantRequest {
                        request_key: key.0.clone(),
                        companion_id: input.companion_id.clone(),
                        kind: input.kind.clone(),
                        connection_id: input.connection_id.clone(),
                        destinations: input.destinations.clone(),
                        data_kinds: input.data_kinds.clone(),
                        purpose: input.purpose.clone(),
                        consent: w::Consent {
                            settings_revision: input.consent.settings_revision,
                            provider_revision: input.consent.provider_revision.clone(),
                            ..Default::default()
                        }
                        .into(),
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn patch_grant(&self, id: &str, input: &GrantPatch) -> Result<GrantResult, Error> {
        decode(
            self.0
                .rpc
                .host
                .update_grant_with_options(
                    w::UpdateGrantRequest {
                        id: id.into(),
                        revision: input.revision,
                        destinations: input
                            .destinations
                            .as_ref()
                            .map(|values| w::StringList {
                                values: values.clone(),
                                ..Default::default()
                            })
                            .into(),
                        data_kinds: input
                            .data_kinds
                            .as_ref()
                            .map(|values| w::StringList {
                                values: values.clone(),
                                ..Default::default()
                            })
                            .into(),
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn revoke_grant(&self, id: &str) -> Result<(), Error> {
        self.0
            .rpc
            .host
            .revoke_grant_with_options(
                w::Id {
                    id: id.into(),
                    ..Default::default()
                },
                options(10),
            )
            .await
            .map_err(rpc_error)?;
        Ok(())
    }
    pub async fn register_credentials(
        &self,
        id: &str,
        input: &CredentialInput,
    ) -> Result<CredentialResult, Error> {
        decode(
            self.0
                .rpc
                .host
                .register_credentials_with_options(
                    w::CredentialRequest {
                        id: id.into(),
                        secret: input.secret.clone(),
                        source: input.source.clone(),
                        ..Default::default()
                    },
                    options(10),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn revoke_credentials(&self, id: &str) -> Result<(), Error> {
        self.0
            .rpc
            .host
            .revoke_credentials_with_options(
                w::Id {
                    id: id.into(),
                    ..Default::default()
                },
                options(10),
            )
            .await
            .map_err(rpc_error)?;
        Ok(())
    }
    pub async fn check(
        &self,
        id: &str,
        input: &CheckInput,
        key: &IdempotencyKey,
    ) -> Result<CheckResult, Error> {
        decode(
            self.0
                .rpc
                .host
                .check_connection_with_options(
                    w::CheckRequest {
                        id: id.into(),
                        request_key: key.0.clone(),
                        companion_id: input.companion_id.clone(),
                        purpose: input.purpose.clone(),
                        model_id: input.model_id.clone(),
                        ..Default::default()
                    },
                    options(95),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn models(&self, id: &str, companion: Option<&str>) -> Result<Vec<Model>, Error> {
        self.0
            .rpc
            .host
            .list_models_with_options(
                w::ModelsRequest {
                    id: id.into(),
                    companion_id: companion.map(Into::into),
                    ..Default::default()
                },
                options(35),
            )
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.0
            .rpc
            .host
            .list_providers_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn availability(
        &self,
        connection: &str,
        companion: &str,
    ) -> Result<ConnectionAvailability, Error> {
        decode(
            self.0
                .rpc
                .host
                .get_availability_with_options(
                    w::AvailabilityRequest {
                        connection_id: connection.into(),
                        companion_id: companion.into(),
                        ..Default::default()
                    },
                    options(5),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn reload_providers(&self) -> Result<Vec<ProviderInfo>, Error> {
        self.0
            .rpc
            .host
            .reload_providers_with_options(w::Empty::default(), options(10))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn enable_provider(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Vec<ProviderInfo>, Error> {
        self.0
            .rpc
            .host
            .enable_provider_with_options(
                w::EnableProviderRequest {
                    id: id.into(),
                    enabled,
                    ..Default::default()
                },
                options(10),
            )
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
}
pub(crate) fn options(seconds: u64) -> CallOptions {
    CallOptions::default().with_timeout(Duration::from_secs(seconds))
}
pub(crate) fn decode<T: RpcConvert>(value: T::Wire) -> Result<T, Error> {
    T::from_wire(value).map_err(|_| Error::Protocol)
}
fn encode<T: RpcConvert>(value: T) -> Result<T::Wire, Error> {
    value.into_wire().map_err(|_| Error::Protocol)
}
pub(crate) fn rpc_error(error: connectrpc::ConnectError) -> Error {
    use base64::Engine;
    if error.code == connectrpc::ErrorCode::Unauthenticated {
        return Error::Unauthorized;
    }
    for detail in error.details {
        if !detail.type_url.ends_with("ene.host.v1.ApiFailure") {
            continue;
        }
        let Some(value) = detail.value else {
            continue;
        };
        let Ok(bytes) = base64::engine::general_purpose::STANDARD_NO_PAD.decode(value) else {
            continue;
        };
        let Ok(failure) = <w::ApiFailure as buffa::Message>::decode_from_slice(&bytes) else {
            continue;
        };
        let Some(wire_error) = failure.error.into_option() else {
            continue;
        };
        if let (Ok(status), Ok(error)) = (u16::try_from(failure.http_status), decode(wire_error)) {
            return Error::Api(Box::new(ApiFailure {
                status,
                body: ApiErrorBody {
                    error,
                    request_id: failure.request_id,
                },
            }));
        }
    }
    let standard = match error.code {
        connectrpc::ErrorCode::PermissionDenied => {
            Some((403, "permission_required", "操作への許可が必要です"))
        }
        connectrpc::ErrorCode::FailedPrecondition => Some((
            409,
            "state_conflict",
            "状態が変わりました。取得し直してください",
        )),
        connectrpc::ErrorCode::NotFound => Some((404, "not_found", "対象が見つかりません")),
        connectrpc::ErrorCode::InvalidArgument => {
            Some((400, "invalid_request", "要求の形式が正しくありません"))
        }
        connectrpc::ErrorCode::Unimplemented => {
            Some((422, "unsupported_capability", "この機能を利用できません"))
        }
        _ => None,
    };
    if let Some((status, code, message)) = standard {
        return Error::Api(Box::new(ApiFailure {
            status,
            body: ApiErrorBody {
                request_id: String::new(),
                error: Failure {
                    code: code.into(),
                    message: message.into(),
                    details: serde_json::json!({}),
                },
            },
        }));
    }
    match error.code {
        connectrpc::ErrorCode::ResourceExhausted => Error::ResourceLimit,
        connectrpc::ErrorCode::Internal => Error::Protocol,
        _ => Error::Transport,
    }
}
impl ApiClient {
    pub fn host_rpc(&self) -> &HostRpcClient {
        &self.0.rpc.host
    }
    pub fn device_rpc(&self) -> &DeviceRpcClient {
        &self.0.rpc.devices
    }
    pub async fn stop(&self, companion: &str) -> Result<(), Error> {
        self.host_rpc()
            .stop_with_options(
                w::StopRequest {
                    companion_id: companion.into(),
                    ..Default::default()
                },
                options(5),
            )
            .await
            .map_err(rpc_error)?;
        Ok(())
    }
}

impl ApiClient {
    pub async fn plugins(&self) -> Result<Vec<ene_api_types::plugin::PluginInfo>, Error> {
        self.0
            .rpc
            .plugins
            .list_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn plugin(&self, id: &str) -> Result<ene_api_types::plugin::PluginInfo, Error> {
        decode(
            self.0
                .rpc
                .plugins
                .get_with_options(
                    w::Id {
                        id: id.into(),
                        ..Default::default()
                    },
                    options(5),
                )
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
    pub async fn reload_plugins(&self) -> Result<Vec<ene_api_types::plugin::PluginInfo>, Error> {
        self.0
            .rpc
            .plugins
            .reload_with_options(w::Empty::default(), options(30))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn enable_plugin(
        &self,
        id: &str,
        input: &ene_api_types::plugin::PluginEnabled,
    ) -> Result<Vec<ene_api_types::plugin::PluginInfo>, Error> {
        self.0
            .rpc
            .plugins
            .enable_with_options(
                w::EnablePluginRequest {
                    id: id.into(),
                    enabled: input.enabled,
                    revision: input.revision.clone(),
                    ..Default::default()
                },
                options(60),
            )
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn plugin_bindings(
        &self,
    ) -> Result<Vec<ene_api_types::plugin::PluginBinding>, Error> {
        self.0
            .rpc
            .plugins
            .list_bindings_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn bind_plugin(
        &self,
        input: &ene_api_types::plugin::PluginBinding,
    ) -> Result<Vec<ene_api_types::plugin::PluginBinding>, Error> {
        self.0
            .rpc
            .plugins
            .bind_with_options(encode(input.clone())?, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items
            .into_iter()
            .map(decode)
            .collect()
    }
    pub async fn invoke_plugin(
        &self,
        input: &ene_api_types::plugin::PluginInvocation,
    ) -> Result<ene_api_types::plugin::PluginResult, Error> {
        decode(
            self.0
                .rpc
                .plugins
                .invoke_with_options(encode(input.clone())?, options(190))
                .await
                .map_err(rpc_error)?
                .into_owned(),
        )
    }
}
