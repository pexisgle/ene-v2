//! Host connection actions, setting projections and event dispatch.
mod connection;
mod events;
mod settings;
mod snapshot;

#[derive(Default)]
pub(super) struct Drafts {
    pub companion_revision: u64,
    pub settings_revision: u64,
    pub assignment_revision: u64,
    pub permission: Option<PermissionDraft>,
}

#[derive(Clone)]
pub(super) struct PermissionDraft {
    pub connection: String,
    pub consent: ene_api_types::GrantConsent,
    pub destinations: Vec<String>,
}

#[cfg(test)]
mod tests;
