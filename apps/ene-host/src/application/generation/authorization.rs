use super::*;

pub(crate) struct AuthorizedContext {
    pub binding: ProviderBinding,
    pub secret: Option<Secret>,
}

pub(crate) fn authorize(
    app: &HostApplication,
    configuration: &Configuration,
    controls: &Controls,
    companion: &str,
    connection: &ProviderConnection,
    operation: OperationKind,
) -> Result<AuthorizedContext, Error> {
    let binding = app.0.providers.resolve(connection)?;
    let policy = binding.policy();
    let secret = controls.secrets.get(&connection.id).cloned();
    let availability = rules::evaluate(
        configuration,
        companion,
        connection,
        operation,
        Some(policy),
        rules::ExecutionState {
            credential_present: secret.is_some(),
            healthy: !controls.failed(),
            stopping: *app.0.shutdown.borrow(),
            busy: false,
        },
    );
    if availability.dependency == "credential_missing" {
        return Err(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("credential_missing"),
        });
    }
    if availability.dependency == "model_unset" {
        return Err(Error {
            kind: ErrorKind::StateConflict,
            reason: Some("model_unset"),
        });
    }
    if availability.authorization != "granted" {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    if !availability.runnable {
        return Err(Error::unavailable());
    }
    Ok(AuthorizedContext { binding, secret })
}

pub(crate) fn register(
    controls: &mut Controls,
    id: String,
    companion: String,
    connection: &ProviderConnection,
    context: &AuthorizedContext,
    operation: OperationKind,
) -> watch::Receiver<bool> {
    let (cancel, receiver) = watch::channel(false);
    controls.running.insert(
        id,
        Running {
            companion,
            connection: connection.id.clone(),
            policy: context.binding.policy().clone(),
            nonce: context.secret.as_ref().map(|secret| secret.nonce.clone()),
            operation,
            cancel,
        },
    );
    receiver
}

/// Linearization gate shared by outbound sends and successful completion.
pub(crate) fn check_run(app: &HostApplication, controls: &Controls, id: &str) -> Result<(), Error> {
    let run = controls
        .running
        .get(id)
        .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
    if *run.cancel.borrow() {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    check_access(
        app,
        controls,
        &run.companion,
        &run.connection,
        &run.policy,
        run.nonce.as_deref(),
        run.operation,
    )
}

/// Whether the configuration still permits this operation under the same
/// connection, policy and credential as when it was accepted. Used by every
/// outbound send and by a kernel turn's authorization callback.
pub(crate) fn check_access(
    app: &HostApplication,
    controls: &Controls,
    companion: &str,
    connection: &str,
    policy: &Policy,
    nonce: Option<&str>,
    operation: OperationKind,
) -> Result<(), Error> {
    if *app.0.shutdown.borrow() || controls.failed() {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let configuration = controls.config.as_ref().ok_or_else(Error::unavailable)?;
    let provider = rules::connection(configuration, connection)
        .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
    let current = app
        .0
        .providers
        .resolve(&provider)
        .map(|binding| binding.policy().clone())
        .map_err(|_| Error::new(ErrorKind::PermissionRequired))?;
    if current.fingerprint != policy.fingerprint
        || controls
            .secrets
            .get(connection)
            .map(|secret| secret.nonce.as_str())
            != nonce
        || (current.requires_credentials && !controls.secrets.contains_key(connection))
    {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let spec = operation.spec();
    if !rules::allowed(
        configuration,
        companion,
        &provider,
        spec.purpose.as_str(),
        spec.data,
        &current,
    ) {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    Ok(())
}

struct Authority {
    app: HostApplication,
    id: String,
}
impl Authorization for Authority {
    fn check(&self) -> BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            let controls = self.app.0.controls.lock().await;
            check_run(&self.app, &controls, &self.id)
        })
    }
}

pub(crate) fn invocation(
    app: &HostApplication,
    connection: ProviderConnection,
    context: AuthorizedContext,
    id: String,
    input: InvocationInput,
    cancel: watch::Receiver<bool>,
) -> Invocation {
    let authorization = std::sync::Arc::new(Authority {
        app: app.clone(),
        id: id.clone(),
    });
    Invocation::new(
        connection,
        input,
        context.secret,
        authorization,
        cancel,
        context.binding,
    )
}
