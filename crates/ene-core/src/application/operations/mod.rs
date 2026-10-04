use super::HostApplication;
use super::generation::*;
use crate::{
    domain::{rules, *},
    ports::{Outcome, Receipt},
};
pub use ene_api_types::host::{
    CheckInput, CompanionPatch, ConnectionInput, GrantInput, GrantPatch, MessageInput,
    SettingsPatch,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::sync::watch;
use uuid::Uuid;
mod checks;
mod configuration;
mod grants;
mod messages;
mod providers;
pub use checks::check;
pub use configuration::{patch_companion, patch_settings};
pub use grants::{delete_grant, patch_grant, post_grant};
pub use messages::accept_message;
