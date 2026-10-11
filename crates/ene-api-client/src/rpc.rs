//! Typed operations on the generated Connect clients.
use super::*;
use connectrpc::client::{CallOptions, ClientConfig, HttpClient};
use ene_rpc as w;
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
        .with_default_header(AUTHORIZATION, bearer);
        Ok(Self {
            host: HostRpcClient::new(transport.clone(), config.clone()),
            plugins: PluginRpcClient::new(transport.clone(), config.clone()),
            devices: DeviceRpcClient::new(transport, config),
        })
    }
}
impl ApiClient {
    pub async fn snapshot(&self, subscription: Option<&str>) -> Result<Snapshot, Error> {
        Ok(self
            .0
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
            .into_owned())
    }
    pub async fn companions(&self) -> Result<Vec<Companion>, Error> {
        Ok(self
            .0
            .rpc
            .host
            .list_companions_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn companion(&self, id: &str) -> Result<Companion, Error> {
        Ok(self
            .0
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
            .into_owned())
    }
    pub async fn patch_companion(
        &self,
        id: &str,
        input: &UpdateCompanionRequest,
    ) -> Result<Companion, Error> {
        let mut request = input.clone();
        request.id = id.into();
        Ok(self
            .0
            .rpc
            .host
            .update_companion_with_options(request, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn messages(
        &self,
        companion: &str,
        cursor: Option<&str>,
        limit: Option<usize>,
    ) -> Result<History, Error> {
        Ok(self
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
            .into_owned())
    }
    pub async fn message(&self, companion: &str, id: &str) -> Result<Message, Error> {
        Ok(self
            .0
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
            .into_owned())
    }
    pub async fn send_message(
        &self,
        companion: &str,
        input: &SendMessageRequest,
        key: &IdempotencyKey,
    ) -> Result<MessageAcceptance, Error> {
        let mut request = input.clone();
        request.companion_id = companion.into();
        request.request_key = key.0.clone();
        Ok(self
            .0
            .rpc
            .host
            .send_message_with_options(request, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn settings(&self) -> Result<Settings, Error> {
        Ok(self
            .0
            .rpc
            .host
            .get_settings_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn patch_settings(&self, input: &UpdateSettingsRequest) -> Result<Settings, Error> {
        Ok(self
            .0
            .rpc
            .host
            .update_settings_with_options(input.clone(), options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn grants(&self, companion: Option<&str>) -> Result<Vec<Grant>, Error> {
        Ok(self
            .0
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
            .items)
    }
    pub async fn create_grant(
        &self,
        input: &CreateGrantRequest,
        key: &IdempotencyKey,
    ) -> Result<GrantResult, Error> {
        let mut request = input.clone();
        request.request_key = key.0.clone();
        Ok(self
            .0
            .rpc
            .host
            .create_grant_with_options(request, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn patch_grant(
        &self,
        id: &str,
        input: &UpdateGrantRequest,
    ) -> Result<GrantResult, Error> {
        let mut request = input.clone();
        request.id = id.into();
        Ok(self
            .0
            .rpc
            .host
            .update_grant_with_options(request, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn register_credentials(
        &self,
        id: &str,
        input: &CredentialRequest,
    ) -> Result<CredentialResult, Error> {
        let mut request = input.clone();
        request.id = id.into();
        Ok(self
            .0
            .rpc
            .host
            .register_credentials_with_options(request, options(10))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn check(
        &self,
        id: &str,
        input: &CheckRequest,
        key: &IdempotencyKey,
    ) -> Result<CheckResult, Error> {
        let mut request = input.clone();
        request.id = id.into();
        request.request_key = key.0.clone();
        Ok(self
            .0
            .rpc
            .host
            .check_connection_with_options(request, options(95))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub async fn models(&self, id: &str, companion: Option<&str>) -> Result<Vec<Model>, Error> {
        Ok(self
            .0
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
            .items)
    }
    pub async fn providers(&self) -> Result<Vec<Provider>, Error> {
        Ok(self
            .0
            .rpc
            .host
            .list_providers_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn availability(
        &self,
        connection: &str,
        companion: &str,
    ) -> Result<Availability, Error> {
        Ok(self
            .0
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
            .into_owned())
    }
    pub async fn reload_providers(&self) -> Result<Vec<Provider>, Error> {
        Ok(self
            .0
            .rpc
            .host
            .reload_providers_with_options(w::Empty::default(), options(10))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn enable_provider(&self, id: &str, enabled: bool) -> Result<Vec<Provider>, Error> {
        Ok(self
            .0
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
            .items)
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
    pub async fn stop(&self, companion: &str) -> Result<(), Error> {
        self.0
            .rpc
            .host
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
    pub async fn plugins(&self) -> Result<Vec<Plugin>, Error> {
        Ok(self
            .0
            .rpc
            .plugins
            .list_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn plugin(&self, id: &str) -> Result<Plugin, Error> {
        Ok(self
            .0
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
            .into_owned())
    }
    pub async fn reload_plugins(&self) -> Result<Vec<Plugin>, Error> {
        Ok(self
            .0
            .rpc
            .plugins
            .reload_with_options(w::Empty::default(), options(30))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn enable_plugin(
        &self,
        id: &str,
        input: &EnablePluginRequest,
    ) -> Result<Vec<Plugin>, Error> {
        let mut request = input.clone();
        request.id = id.into();
        Ok(self
            .0
            .rpc
            .plugins
            .enable_with_options(request, options(60))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn plugin_bindings(&self) -> Result<Vec<PluginBinding>, Error> {
        Ok(self
            .0
            .rpc
            .plugins
            .list_bindings_with_options(w::Empty::default(), options(5))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn bind_plugin(&self, input: &PluginBinding) -> Result<Vec<PluginBinding>, Error> {
        Ok(self
            .0
            .rpc
            .plugins
            .bind_with_options(input.clone(), options(10))
            .await
            .map_err(rpc_error)?
            .into_owned()
            .items)
    }
    pub async fn invoke_plugin(&self, input: &PluginInvocation) -> Result<PluginResult, Error> {
        Ok(self
            .0
            .rpc
            .plugins
            .invoke_with_options(input.clone(), options(190))
            .await
            .map_err(rpc_error)?
            .into_owned())
    }
    pub fn host_rpc(&self) -> &HostRpcClient {
        &self.0.rpc.host
    }
    pub fn device_rpc(&self) -> &DeviceRpcClient {
        &self.0.rpc.devices
    }
}
pub(crate) fn options(seconds: u64) -> CallOptions {
    CallOptions::default().with_timeout(Duration::from_secs(seconds))
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
        if failure.error.is_unset() {
            continue;
        }
        if let Ok(status) = u16::try_from(failure.http_status) {
            return Error::Api(Box::new(ApiFailure {
                status,
                body: failure,
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
            body: w::ApiFailure {
                http_status: u32::from(status),
                error: Failure {
                    code: code.into(),
                    message: message.into(),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            },
        }));
    }
    match error.code {
        connectrpc::ErrorCode::ResourceExhausted => Error::ResourceLimit,
        connectrpc::ErrorCode::Internal => Error::Protocol,
        _ => Error::Transport,
    }
}
