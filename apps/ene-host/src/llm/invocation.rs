//! Credential-bearing execution; signing is private to this module and its transport.
use super::*;
use crate::ports::Secret;
use tokio::sync::watch;

mod transport;
pub use transport::{AuthorizedTransport, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Response};

struct RestrictedAuthorization {
    original: Arc<dyn Authorization>,
    scope: Arc<dyn Authorization>,
}
impl Authorization for RestrictedAuthorization {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            self.original.check().await?;
            self.scope.check().await
        })
    }
}
pub(crate) enum InvocationInput {
    Conversation(Generation),
    ConnectionCheck(Generation),
    ModelList,
    Task(Generation),
}
impl InvocationInput {
    fn operation(&self) -> OperationKind {
        match self {
            Self::Conversation(_) => OperationKind::Conversation,
            Self::ConnectionCheck(_) => OperationKind::ConnectionCheck,
            Self::ModelList => OperationKind::ModelList,
            Self::Task(_) => OperationKind::Task,
        }
    }
}
/// Adapters can only submit unsigned requests to `AuthorizedTransport`.
/// Signing cannot be used independently to inspect a credential-bearing request.
/// ```compile_fail
/// use ene_host::llm::{Invocation, CredentialPlacement};
/// async fn extract(invocation: &Invocation, request: &mut http::Request<bytes::Bytes>) {
///     invocation.authorize_outbound(request, &CredentialPlacement::Bearer).await.unwrap();
/// }
/// ```
pub struct Invocation {
    id: String,
    connection: Connection,
    input: InvocationInput,
    secret: Option<Secret>,
    authorization: Arc<dyn Authorization>,
    cancel: watch::Receiver<bool>,
    binding: ProviderBinding,
}

#[derive(Clone)]
pub enum CredentialPlacement {
    None,
    Bearer,
    Header(String),
    Query(String),
}
#[derive(Clone)]
pub(crate) enum TransportScope {
    Provider,
    Catalog(url::Url),
}

impl Invocation {
    pub(crate) fn new(
        id: String,
        connection: Connection,
        input: InvocationInput,
        secret: Option<Secret>,
        authorization: Arc<dyn Authorization>,
        cancel: watch::Receiver<bool>,
        binding: ProviderBinding,
    ) -> Self {
        Self {
            id,
            connection,
            input,
            secret,
            authorization,
            cancel,
            binding,
        }
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    /// Add runtime revocation without replacing the host's original authorization.
    pub fn restrict(mut self, scope: Arc<dyn Authorization>) -> Self {
        self.authorization = Arc::new(RestrictedAuthorization {
            original: self.authorization,
            scope,
        });
        self
    }
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn generation(&self) -> Result<&Generation> {
        match &self.input {
            InvocationInput::Conversation(request)
            | InvocationInput::ConnectionCheck(request)
            | InvocationInput::Task(request) => Ok(request),
            InvocationInput::ModelList => Err(Error::invalid()),
        }
    }
    pub fn authorization(&self) -> Arc<dyn Authorization> {
        self.authorization.clone()
    }
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancel.clone()
    }
    pub fn operation(&self) -> OperationKind {
        self.input.operation()
    }
    pub fn policy(&self) -> &Policy {
        self.binding.policy()
    }

    // Only the host-owned transport may sign requests.
    async fn authorize_outbound(
        &self,
        request: &mut http::Request<bytes::Bytes>,
        placement: &CredentialPlacement,
        scope: &TransportScope,
    ) -> Result<()> {
        self.authorization.check().await?;
        let denied = || Error::new(ErrorKind::PermissionRequired);
        let base = url::Url::parse(&self.connection.endpoint).map_err(|_| denied())?;
        let mut target = url::Url::parse(&request.uri().to_string()).map_err(|_| denied())?;
        if !target.username().is_empty()
            || target.password().is_some()
            || target.fragment().is_some()
            || !self
                .policy()
                .destinations
                .contains(&target.origin().ascii_serialization())
        {
            return Err(denied());
        }
        match scope {
            TransportScope::Catalog(source) => {
                if target.as_str() != source.as_str()
                    || *request.method() != http::Method::GET
                    || !request.body().is_empty()
                    || !self
                        .policy()
                        .destinations
                        .contains(&source.origin().ascii_serialization())
                {
                    return Err(denied());
                }
            }
            TransportScope::Provider => {
                if target.origin() != base.origin() {
                    return Err(denied());
                }
                let base_path = base.path().trim_end_matches('/');
                let path = target.path();
                let allowed = match self.operation() {
                    OperationKind::ModelList => {
                        *request.method() == http::Method::GET
                            && request.body().is_empty()
                            && path == format!("{base_path}/models")
                    }
                    OperationKind::Conversation
                    | OperationKind::ConnectionCheck
                    | OperationKind::Task => {
                        *request.method() == http::Method::POST
                            && (path == format!("{base_path}/chat/completions")
                                || path == format!("{base_path}/responses"))
                    }
                };
                if !allowed {
                    return Err(denied());
                }
            }
        }
        // SDK-generated auth is replaced; it can never be forwarded to a public catalog.
        request.headers_mut().remove(http::header::AUTHORIZATION);
        if matches!(scope, TransportScope::Catalog(_)) {
            return Ok(());
        }
        let secret = || self.secret.as_ref().ok_or_else(denied);
        let header = |value: &str| {
            let mut header = http::HeaderValue::from_str(value).map_err(|_| denied())?;
            header.set_sensitive(true);
            Ok::<_, Error>(header)
        };
        match placement {
            CredentialPlacement::None => {}
            CredentialPlacement::Bearer => {
                request.headers_mut().insert(
                    http::header::AUTHORIZATION,
                    header(&format!("Bearer {}", secret()?.value))?,
                );
            }
            CredentialPlacement::Header(name) => {
                let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| denied())?;
                request
                    .headers_mut()
                    .insert(name, header(&secret()?.value)?);
            }
            CredentialPlacement::Query(name) => {
                if target.query_pairs().any(|(key, _)| key == name.as_str()) {
                    return Err(denied());
                }
                target.query_pairs_mut().append_pair(name, &secret()?.value);
                *request.uri_mut() = target.as_str().parse().map_err(|_| denied())?;
            }
        }
        Ok(())
    }
    pub async fn generate(self) -> Result<BoxStream<'static, Result<Event>>> {
        if self.operation() == OperationKind::ModelList {
            return Err(Error::invalid());
        }
        self.authorization.check().await?;
        self.binding.provider.clone().generate(self).await
    }
    pub async fn models(self) -> Result<Vec<Model>> {
        if self.operation() != OperationKind::ModelList {
            return Err(Error::invalid());
        }
        self.authorization.check().await?;
        let models = self.binding.provider.clone().models(self).await?;
        validate_models(&models)?;
        Ok(models)
    }
}
