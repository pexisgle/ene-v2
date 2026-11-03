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
    /// Only the transport's contract tests build a one-shot conversation
    /// invocation now that the kernel sends its own requests.
    #[cfg(test)]
    Conversation(Generation),
    ConnectionCheck(Generation),
    ModelList,
    Task(Generation),
}
impl InvocationInput {
    fn operation(&self) -> OperationKind {
        match self {
            #[cfg(test)]
            Self::Conversation(_) => OperationKind::Conversation,
            Self::ConnectionCheck(_) => OperationKind::ConnectionCheck,
            Self::ModelList => OperationKind::ModelList,
            Self::Task(_) => OperationKind::Task,
        }
    }
}

/// The host-owned authority for one outbound operation: what may be sent,
/// where, under which credential and grant. The request itself is separate,
/// so an execution kernel can build its own requests and still go through the
/// one signing and destination-checking boundary.
pub(crate) struct Outbound {
    id: String,
    connection: Connection,
    secret: Option<Secret>,
    authorization: Arc<dyn Authorization>,
    binding: ProviderBinding,
    operation: OperationKind,
}

impl Outbound {
    pub(crate) fn new(
        id: String,
        connection: Connection,
        secret: Option<Secret>,
        authorization: Arc<dyn Authorization>,
        binding: ProviderBinding,
        operation: OperationKind,
    ) -> Self {
        Self {
            id,
            connection,
            secret,
            authorization,
            binding,
            operation,
        }
    }
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
    pub(crate) fn policy(&self) -> &Policy {
        self.binding.policy()
    }
    pub(crate) fn authorization(&self) -> Arc<dyn Authorization> {
        self.authorization.clone()
    }
    pub(crate) fn operation(&self) -> OperationKind {
        self.operation
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
                let allowed = match self.operation {
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
}

/// One credential-bearing request of the host's own adapters. Adapters can
/// only submit unsigned requests to [`AuthorizedTransport`]; the signing
/// method is private to this module and its transport.
pub struct Invocation {
    outbound: Arc<Outbound>,
    input: InvocationInput,
    cancel: watch::Receiver<bool>,
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
        let outbound = Outbound::new(
            id,
            connection,
            secret,
            authorization,
            binding,
            input.operation(),
        );
        Self {
            outbound: Arc::new(outbound),
            input,
            cancel,
        }
    }
    pub fn id(&self) -> &str {
        &self.outbound.id
    }
    /// Add runtime revocation without replacing the host's original authorization.
    pub fn restrict(mut self, scope: Arc<dyn Authorization>) -> Self {
        let outbound = Self::restricted(&self.outbound, scope);
        self.outbound = outbound;
        self
    }
    fn restricted(outbound: &Outbound, scope: Arc<dyn Authorization>) -> Arc<Outbound> {
        Arc::new(Outbound {
            id: outbound.id.clone(),
            connection: outbound.connection.clone(),
            secret: outbound.secret.clone(),
            authorization: Arc::new(RestrictedAuthorization {
                original: outbound.authorization.clone(),
                scope,
            }),
            binding: outbound.binding.clone(),
            operation: outbound.operation,
        })
    }
    pub fn connection(&self) -> &Connection {
        &self.outbound.connection
    }
    pub fn generation(&self) -> Result<&Generation> {
        match &self.input {
            #[cfg(test)]
            InvocationInput::Conversation(request) => Ok(request),
            InvocationInput::ConnectionCheck(request) | InvocationInput::Task(request) => {
                Ok(request)
            }
            InvocationInput::ModelList => Err(Error::invalid()),
        }
    }
    pub fn authorization(&self) -> Arc<dyn Authorization> {
        self.outbound.authorization.clone()
    }
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancel.clone()
    }
    pub fn operation(&self) -> OperationKind {
        self.input.operation()
    }
    pub fn policy(&self) -> &Policy {
        self.outbound.policy()
    }
    pub(crate) fn outbound(&self) -> Arc<Outbound> {
        self.outbound.clone()
    }
    pub async fn generate(self) -> Result<BoxStream<'static, Result<Event>>> {
        if self.operation() == OperationKind::ModelList {
            return Err(Error::invalid());
        }
        self.outbound.authorization.check().await?;
        self.outbound.binding.provider.clone().generate(self).await
    }
    pub async fn models(self) -> Result<Vec<Model>> {
        if self.operation() != OperationKind::ModelList {
            return Err(Error::invalid());
        }
        self.outbound.authorization.check().await?;
        let models = self.outbound.binding.provider.clone().models(self).await?;
        validate_models(&models)?;
        Ok(models)
    }
}
