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

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn check(
    app: HostApplication,
    id: String,
    key: String,
    request: CheckInput,
) -> Result<(Outcome, Value), Error> {
    let owner = app.clone();
    owner.execute(checks::check(app, id, key, request)).await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn patch_settings(
    app: HostApplication,
    request: SettingsPatch,
) -> Result<Settings, Error> {
    let owner = app.clone();
    owner
        .execute(configuration::patch_settings(app, request))
        .await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn patch_companion(
    app: HostApplication,
    id: String,
    request: CompanionPatch,
) -> Result<Companion, Error> {
    app.configure(configuration::PatchCompanion { id, request })
        .await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn delete_grant(app: HostApplication, id: String) -> Result<(), Error> {
    let owner = app.clone();
    owner.execute(grants::delete_grant(app, id)).await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn patch_grant(
    app: HostApplication,
    id: String,
    request: GrantPatch,
) -> Result<ene_api_types::GrantResult, Error> {
    let owner = app.clone();
    owner.execute(grants::patch_grant(app, id, request)).await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn post_grant(
    app: HostApplication,
    key: String,
    request: GrantInput,
) -> Result<(Outcome, Value), Error> {
    let owner = app.clone();
    owner.execute(grants::post_grant(app, key, request)).await
}

/// Host-owned command: finishes its commit even if the caller disconnects.
pub async fn accept_message(
    app: HostApplication,
    id: String,
    key: String,
    request: MessageInput,
) -> Result<(Outcome, Value), Error> {
    let owner = app.clone();
    owner
        .execute(messages::accept_message(app, id, key, request))
        .await
}
