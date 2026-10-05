//! One commit boundary for settings, companions, grants and idempotency receipts.
use super::{
    HostApplication,
    changes::{Change, publish},
    generation::{Controls, cancel_unpermitted, refresh},
};
use crate::{domain::*, ports::ReceiptWrite};
use serde::Serialize;

pub enum ConfigurationTarget {
    Companion { id: String, revision: u64 },
    Settings { revision: u64 },
    Grant { id: String, revision: u64 },
}
impl ConfigurationTarget {
    fn revision<'a>(&self, c: &'a mut Configuration) -> Result<&'a mut u64, Error> {
        match self {
            Self::Companion { id, .. } => c.companions.get_mut(id).map(|c| &mut c.revision),
            Self::Settings { .. } => Some(&mut c.settings.revision),
            Self::Grant { id, .. } => c
                .grants
                .iter_mut()
                .find(|g| &g.id == id)
                .map(|g| &mut g.revision),
        }
        .ok_or_else(|| Error::new(ErrorKind::NotFound))
    }
    fn expected(&self) -> u64 {
        match self {
            Self::Companion { revision, .. }
            | Self::Settings { revision }
            | Self::Grant { revision, .. } => *revision,
        }
    }
    fn validate(&self, c: &mut Configuration) -> Result<(), Error> {
        if *self.revision(c)? != self.expected() {
            return Err(Error::new(ErrorKind::RevisionConflict));
        }
        Ok(())
    }
    fn advance(&self, c: &mut Configuration) -> Result<(), Error> {
        *self.revision(c)? = self
            .expected()
            .checked_add(1)
            .ok_or_else(|| Error::new(ErrorKind::StateConflict))?;
        Ok(())
    }
}

/// A synchronous domain change. The host supplies admission, locking, storage,
/// authority cancellation and notifications. External I/O belongs in a port.
pub trait ConfigurationCommand: Send + 'static {
    type Output: Send + 'static;
    fn target(&self) -> ConfigurationTarget;
    fn apply(
        &self,
        configuration: &mut Configuration,
        providers: &dyn crate::ports::llm::ProviderRegistry,
    ) -> Result<(), Error>;
    fn output(&self, configuration: &Configuration) -> Result<Self::Output, Error>;
}

impl HostApplication {
    pub async fn configure<C: ConfigurationCommand>(&self, command: C) -> Result<C::Output, Error> {
        let app = self.clone();
        self.execute(async move {
            let mut controls = app.0.controls.lock().await;
            let mut next = super::generation::config(&app, &mut controls).await?;
            let target = command.target();
            target.validate(&mut next)?;
            command.apply(&mut next, app.0.providers.as_ref())?;
            target.advance(&mut next)?;
            commit_with(&app, &mut controls, next, None, move |c| command.output(c)).await
        })
        .await
    }
}

pub(super) async fn commit(
    app: &HostApplication,
    controls: &mut Controls,
    next: Configuration,
    receipt: Option<ReceiptWrite>,
) -> Result<(), Error> {
    commit_with(app, controls, next, receipt, |_| Ok(())).await
}

async fn commit_with<T>(
    app: &HostApplication,
    controls: &mut Controls,
    mut next: Configuration,
    receipt: Option<ReceiptWrite>,
    output: impl FnOnce(&Configuration) -> Result<T, Error>,
) -> Result<T, Error> {
    validate_structure(controls.config.as_ref(), &next)?;
    refresh(app, &mut next);
    let output = output(&next)?;
    let previous = controls.config.as_ref();
    let mut changes = Vec::new();
    for companion in next.companions.values() {
        if different(
            previous.and_then(|c| c.companions.get(&companion.id)),
            companion,
        )? {
            changes.push(Change::Companion(companion.clone()));
        }
    }
    if different(previous.map(|c| &c.settings), &next.settings)? {
        changes.push(Change::Settings(next.settings.clone()));
    }
    if different(previous.map(|c| &c.grants), &next.grants)? {
        changes.push(Change::Grants(next.grants.clone()));
    }
    // Snapshots cannot observe a commit with the cursor preceding its events.
    let mut boundary = app.0.boundary.lock().await;
    app.0
        .repository
        .save_configuration(next.clone(), receipt)
        .await?;
    cancel_unpermitted(app, controls, &next);
    controls.config = Some(next);
    for change in changes {
        publish(&mut boundary, app, change);
    }
    Ok(output)
}

fn different<T: Serialize>(previous: Option<&T>, next: &T) -> Result<bool, Error> {
    let value = |v| serde_json::to_value(v).map_err(|_| Error::new(ErrorKind::StorageFailure));
    Ok(previous.map(value).transpose()? != Some(value(next)?))
}

fn validate_structure(previous: Option<&Configuration>, next: &Configuration) -> Result<(), Error> {
    // Companion removal needs an explicit repository operation and deletion event.
    // The current configuration writer upserts companions, so never silently resurrect one.
    if previous.is_some_and(|old| {
        old.companions
            .keys()
            .any(|id| !next.companions.contains_key(id))
    }) {
        return Err(Error::new(ErrorKind::UnsupportedCapability));
    }
    let mut connections = std::collections::HashSet::new();
    if next
        .settings
        .connections
        .iter()
        .any(|p| p.id.is_empty() || !connections.insert(p.id.as_str()))
        || next.companions.iter().any(|(id, c)| {
            id != &c.id
                || [&c.model_assignments.companion, &c.model_assignments.task]
                    .into_iter()
                    .flatten()
                    .any(|a| !connections.contains(a.connection_id.as_str()))
        })
        || next
            .credential_refs
            .keys()
            .any(|id| !connections.contains(id.as_str()))
        || next.grants.iter().any(|g| {
            g.status == GrantStatus::Active
                && (!next.companions.contains_key(&g.companion_id)
                    || !connections.contains(g.connection_id.as_str()))
        })
    {
        return Err(Error::new(ErrorKind::StateConflict));
    }
    Ok(())
}
