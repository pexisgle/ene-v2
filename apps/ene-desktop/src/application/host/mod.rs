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

#[derive(Default)]
pub(super) enum AvailabilityRequest {
    #[default]
    Idle,
    Pending {
        id: String,
        signature: serde_json::Value,
    },
    Ready(serde_json::Value),
    Failed(serde_json::Value),
}
impl AvailabilityRequest {
    pub fn signature(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Idle => None,
            Self::Pending { signature, .. } | Self::Ready(signature) | Self::Failed(signature) => {
                Some(signature)
            }
        }
    }
}
