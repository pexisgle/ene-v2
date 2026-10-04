use super::*;
pub(crate) fn permitted(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    companion_id: &str,
    p: &ProviderConnection,
    purpose: &str,
) -> Result<Option<Secret>, Error> {
    let data = if purpose == "conversation" {
        vec!["conversation"]
    } else {
        vec!["test_input"]
    };
    permitted_data(app, c, controls, companion_id, p, purpose, &data)
}
pub(crate) fn permitted_data(
    app: &HostApplication,
    c: &Configuration,
    controls: &Controls,
    companion_id: &str,
    p: &ProviderConnection,
    purpose: &str,
    data: &[&str],
) -> Result<Option<Secret>, Error> {
    let policy = app.0.providers.policy(p)?;
    let secret = controls.secrets.get(&p.id).cloned();
    if policy.requires_credentials && secret.is_none() {
        return Err(Error {
            kind: ErrorKind::Unavailable,
            reason: Some("credential_missing"),
        });
    }
    if !rules::allowed(c, companion_id, p, purpose, data, &policy) {
        return Err(Error::new(ErrorKind::PermissionRequired));
    }
    Ok(secret)
}
/// Rechecks the host-selected context at every external send. Plugins cannot supply purpose.
struct Authority {
    app: HostApplication,
    id: String,
    connection: ProviderConnection,
    purpose: &'static str,
    nonce: Option<String>,
    fingerprint: Value,
    data: Vec<&'static str>,
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
            if policy.fingerprint != self.fingerprint
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
                self.purpose,
                &self.data,
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
    fn purpose(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::ConnectionCheck | Self::ModelList => "connection_check",
        }
    }
    fn data(self) -> Vec<&'static str> {
        vec![match self {
            Self::Conversation => "conversation",
            Self::ConnectionCheck => "test_input",
            Self::ModelList => "metadata",
        }]
    }
}

pub(in crate::application) fn invocation(
    app: &HostApplication,
    p: ProviderConnection,
    secret: Option<Secret>,
    id: String,
    request: Generation,
    operation: Operation,
    cancel: watch::Receiver<bool>,
) -> Invocation {
    let authorization = std::sync::Arc::new(Authority {
        app: app.clone(),
        id: id.clone(),
        connection: p.clone(),
        purpose: operation.purpose(),
        nonce: secret.as_ref().map(|s| s.nonce.clone()),
        fingerprint: fingerprint(&p, app.0.providers.as_ref()),
        data: operation.data(),
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
