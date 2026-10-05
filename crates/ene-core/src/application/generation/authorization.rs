use super::*;
/// Prepare admission, authority and cancellation from the same operation definition.
pub(in crate::application) struct PreparedRun {
    pub running: Running,
    pub secret: Option<Secret>,
    pub cancelled: watch::Receiver<bool>,
}
pub(in crate::application) fn prepare(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    companion: &str,
    p: &ProviderConnection,
    operation: Operation,
) -> Result<PreparedRun, Error> {
    if *app.0.shutdown.borrow() || app.0.generations.is_closed() {
        return Err(Error::unavailable());
    }
    if controls
        .running
        .values()
        .any(|r| r.terminal_error.is_some())
    {
        return Err(Error::new(ErrorKind::StorageFailure));
    }
    if controls.running.len() >= 8 {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    let (policy, secret) = authorize(app, c, controls, companion, p, operation)?;
    let (cancel, cancelled) = watch::channel(false);
    Ok(PreparedRun {
        running: Running {
            companion: companion.into(),
            connection: p.id.clone(),
            policy,
            terminal_error: None,
            operation,
            cancel,
        },
        secret,
        cancelled,
    })
}

pub(in crate::application) fn authorize(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    companion: &str,
    p: &ProviderConnection,
    operation: Operation,
) -> Result<(Policy, Option<Secret>), Error> {
    let policy = app.0.providers.policy(p)?;
    let secret = controls.secrets.get(&p.id).cloned();
    if policy.requires_credentials && secret.is_none() {
        return Err(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("credential_missing"),
        });
    }
    if !rules::allowed(
        c,
        companion,
        p,
        operation.purpose(),
        operation.data(),
        &policy,
    ) {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    Ok((policy, secret))
}

/// Rechecks the host-selected context at every external send. Plugins cannot supply purpose.
struct Authority {
    app: HostApplication,
    id: String,
    connection: ProviderConnection,
    nonce: Option<String>,
}
impl Authorization for Authority {
    fn check(&self) -> BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            let controls = self.app.0.controls.lock().await;
            let c = controls.config.as_ref().ok_or_else(Error::unavailable)?;
            let run = controls
                .running
                .get(&self.id)
                .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
            let current = rules::connection(c, &self.connection.id)
                .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
            if *run.cancel.borrow() || *self.app.0.shutdown.borrow() {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            // Registry policy is memory-only; bundle verification belongs to process launch.
            let policy = self
                .app
                .0
                .providers
                .policy(&current)
                .map_err(|_| Error::new(ErrorKind::PermissionRequired))?;
            if policy.fingerprint != run.policy.fingerprint
                || controls.secrets.get(&current.id).map(|s| s.nonce.clone()) != self.nonce
            {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            if policy.requires_credentials && !controls.secrets.contains_key(&current.id) {
                return Err(Error {
                    kind: ErrorKind::Unavailable,
                    reason: Some("credential_missing"),
                });
            }
            if !rules::allowed(
                c,
                &run.companion,
                &current,
                run.operation.purpose(),
                run.operation.data(),
                &policy,
            ) {
                return Err(Error::new(ErrorKind::PermissionRequired));
            }
            Ok(())
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::application) enum Operation {
    Conversation,
    ConnectionCheck,
    ModelList,
}
impl Operation {
    pub(in crate::application) fn purpose(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::ConnectionCheck | Self::ModelList => "connection_check",
        }
    }
    pub(in crate::application) fn data(self) -> &'static [&'static str] {
        match self {
            Self::Conversation => &["conversation"],
            Self::ConnectionCheck => &["test_input"],
            Self::ModelList => &["metadata"],
        }
    }
    pub(in crate::application) fn timeout(self) -> Duration {
        Duration::from_secs(match self {
            Self::Conversation => 180,
            Self::ConnectionCheck => 90,
            Self::ModelList => 30,
        })
    }
}

pub(in crate::application) fn invocation(
    app: &HostApplication,
    p: ProviderConnection,
    secret: Option<Secret>,
    id: String,
    request: Generation,
    cancel: watch::Receiver<bool>,
) -> Invocation {
    let authorization = std::sync::Arc::new(Authority {
        app: app.clone(),
        id: id.clone(),
        connection: p.clone(),
        nonce: secret.as_ref().map(|s| s.nonce.clone()),
    });
    Invocation {
        id,
        connection: p,
        secret,
        request,
        authorization,
        cancel,
    }
}
