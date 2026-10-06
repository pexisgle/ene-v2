use super::*;

pub(in crate::application) struct AuthorizedContext {
    pub binding: ProviderBinding,
    pub secret: Option<Secret>,
}

pub(in crate::application) fn authorize(
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
    if availability.dependency == ene_api_types::host::DependencyState::CredentialMissing {
        return Err(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("credential_missing"),
        });
    }
    if availability.dependency == ene_api_types::host::DependencyState::ModelUnset {
        return Err(Error {
            kind: ErrorKind::StateConflict,
            reason: Some("model_unset"),
        });
    }
    if availability.authorization != ene_api_types::host::AuthorizationState::Granted {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    if !availability.runnable {
        return Err(Error::unavailable());
    }
    Ok(AuthorizedContext { binding, secret })
}

pub(in crate::application) fn register(
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
pub(in crate::application) fn check_run(
    app: &HostApplication,
    controls: &Controls,
    id: &str,
) -> Result<(), Error> {
    let c = controls.config.as_ref().ok_or_else(Error::unavailable)?;
    let run = controls
        .running
        .get(id)
        .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
    if *run.cancel.borrow() || *app.0.shutdown.borrow() || controls.failed() {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let connection = rules::connection(c, &run.connection)
        .ok_or_else(|| Error::new(ErrorKind::PermissionRequired))?;
    let policy = app
        .0
        .providers
        .policy(&connection)
        .map_err(|_| Error::new(ErrorKind::PermissionRequired))?;
    if policy.fingerprint != run.policy.fingerprint
        || controls
            .secrets
            .get(&connection.id)
            .map(|secret| &secret.nonce)
            != run.nonce.as_ref()
        || (policy.requires_credentials && !controls.secrets.contains_key(&connection.id))
    {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    let spec = run.operation.spec();
    if !rules::allowed(
        c,
        &run.companion,
        &connection,
        spec.purpose.as_str(),
        spec.data,
        &policy,
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

pub(in crate::application) fn invocation(
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
        id,
        connection,
        input,
        context.secret,
        authorization,
        cancel,
        context.binding,
    )
}
