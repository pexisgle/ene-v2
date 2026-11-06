use super::HostApplication;
use super::generation::*;
use crate::llm::InvocationInput;
use crate::{
    domain::{rules, *},
    ports::{Answer, Outcome, Receipt},
};
pub use ene_rpc::{
    CheckRequest as CheckInput, CreateGrantRequest as GrantInput,
    SendMessageRequest as MessageInput, UpdateCompanionRequest as CompanionPatch,
    UpdateGrantRequest as GrantPatch, UpdateSettingsRequest as SettingsPatch,
};
use serde_json::json;
use tokio::sync::oneshot;
use uuid::Uuid;
mod checks;
mod configuration;
mod grants;
mod providers;
pub use checks::check;
pub use configuration::{patch_companion, patch_settings};
pub use grants::{delete_grant, patch_grant, post_grant};
