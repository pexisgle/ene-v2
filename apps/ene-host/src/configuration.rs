use crate::voice::VoiceCommands as _;

use crate::{
    actor::{Actor, id},
    error::{conflict, invalid, missing},
    model::check_endpoint,
    store,
};
use anyhow::Result;
use ene_rpc as w;
use serde::{Serialize, de::DeserializeOwned};
use sqlx::SqlitePool;

pub(crate) fn identifier(value: &str) -> Result<&str> {
    ene_domain::validate_id(value).map_err(|_| invalid("invalid identifier"))?;
    Ok(value)
}

pub(crate) async fn save<T: Serialize + DeserializeOwned + Send + Sync + Unpin + 'static>(
    db: &SqlitePool,
    kind: &str,
    key: &w::Mutation,
    id: &str,
    hash: &str,
    value: T,
) -> Result<T> {
    let mut tx = db.begin().await?;
    if let Some(replayed) = store::replay(&mut tx, &key.request_key, hash).await? {
        return Ok(replayed);
    }
    store::put(&mut tx, kind, id, key.revision, &value).await?;
    store::acknowledge(&mut tx, &key.request_key, hash, &value).await?;
    tx.commit().await?;
    Ok(value)
}

pub(crate) trait ConfigurationCommands {
    async fn settings(&self) -> Result<w::Settings>;
    async fn put_character(&mut self, request: w::CharacterChange) -> Result<w::Character>;
    async fn put_companion(&mut self, request: w::CompanionChange) -> Result<w::Companion>;
    async fn put_connection(&mut self, request: w::ConnectionChange) -> Result<w::Connection>;
    async fn put_policy(&mut self, request: w::PolicyChange) -> Result<w::Policy>;
    async fn set_credential(&mut self, request: w::CredentialChange) -> Result<w::Empty>;
    async fn delete_record(
        &mut self,
        kind: &'static str,
        request: w::DeleteRequest,
    ) -> Result<w::Empty>;
}

impl ConfigurationCommands for Actor {
    async fn settings(&self) -> Result<w::Settings> {
        let mut db = self.db.acquire().await?;
        Ok(w::Settings {
            connections: store::list(&mut db, "connection").await?,
            policy: Some(
                store::get(&mut db, "policy", "owner")
                    .await?
                    .unwrap_or_default(),
            )
            .into(),
            rules: store::list(&mut db, "rule").await?,
            plugins: store::list(&mut db, "plugin").await?,
            ..Default::default()
        })
    }

    async fn put_character(&mut self, request: w::CharacterChange) -> Result<w::Character> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_character", &request))?;
        let mut value = request
            .character
            .into_option()
            .ok_or_else(|| invalid("character required"))?;
        let profile = value
            .profile
            .as_option()
            .ok_or_else(|| invalid("profile required"))?;
        validate_profile(profile)?;
        if value.id.is_empty() {
            value.id = id();
        }
        identifier(&value.id)?;
        value.revision = key.revision + 1;
        let identifier = value.id.clone();
        let saved = save(&self.db, "character", key, &identifier, &hash, value).await?;
        self.announce_snapshot().await?;
        Ok(saved)
    }

    async fn put_companion(&mut self, request: w::CompanionChange) -> Result<w::Companion> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_companion", &request))?;
        let mut value = request
            .companion
            .into_option()
            .ok_or_else(|| invalid("companion required"))?;
        if value.id.is_empty() {
            value.id = id();
        }
        identifier(&value.id)?;
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        let character = store::get::<w::Character>(&mut db, "character", &value.character_id)
            .await?
            .ok_or_else(|| missing("character not found"))?;
        if value.name.trim().is_empty() {
            value.name = character.profile.name.clone();
        }
        let mut roles = std::collections::HashSet::new();
        for model in &value.models {
            if model.model.is_empty()
                || !roles.insert(model.role)
                || model
                    .role
                    .as_known()
                    .is_none_or(|role| role == w::model_selection::Role::Unspecified)
            {
                return Err(invalid("model role must be assigned once"));
            }
            if store::get::<w::Connection>(&mut db, "connection", &model.connection_id)
                .await?
                .is_none()
            {
                return Err(missing("provider not found"));
            }
        }
        drop(db);
        value.revision = key.revision + 1;
        let identifier = value.id.clone();
        let saved = save(&self.db, "companion", key, &identifier, &hash, value).await?;
        self.announce_snapshot().await?;
        Ok(saved)
    }

    async fn put_connection(&mut self, request: w::ConnectionChange) -> Result<w::Connection> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_connection", &request))?;
        let mut value = request
            .connection
            .into_option()
            .ok_or_else(|| invalid("provider required"))?;
        if value.id.is_empty() {
            value.id = id();
        }
        let connection_id = ene_domain::ConnectionId::try_from(value.id.clone())
            .map_err(|_| invalid("invalid connection identifier"))?;
        if value.name.trim().is_empty() {
            return Err(invalid("provider name required"));
        }
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        match value.kind.as_known() {
            Some(
                w::ProviderKind::OpenaiCompatible
                | w::ProviderKind::McpHttp
                | w::ProviderKind::Voicevox,
            ) => {
                check_endpoint(&value.endpoint)?;
            }
            Some(w::ProviderKind::Search) => {
                crate::web_search::Adapter::parse(&value.adapter)?;
                check_endpoint(&value.endpoint)?;
                if value.enabled
                    && store::list::<w::Connection>(&mut db, "connection")
                        .await?
                        .iter()
                        .any(|other| {
                            other.id != value.id
                                && other.enabled
                                && other.kind == w::ProviderKind::Search
                        })
                {
                    return Err(crate::error::conflict(
                        "disable the current search connection first",
                    ));
                }
            }
            Some(w::ProviderKind::Native) => {
                if value.adapter == ene_domain::agent_platform::ADAPTER {
                    crate::model::google_publisher(&value)?;
                } else {
                    crate::model::native_provider(&value.adapter)?;
                    check_endpoint(&value.endpoint)?;
                }
            }
            Some(w::ProviderKind::Plugin)
                if !value.plugin_id.is_empty() && !value.entrypoint.is_empty() =>
            {
                if !value.endpoint.is_empty() {
                    check_endpoint(&value.endpoint)?;
                }
            }
            Some(w::ProviderKind::McpStdio) if !value.command.trim().is_empty() => {}
            _ => return Err(invalid("unsupported provider configuration")),
        }
        crate::mcp::validate_scope(&value).map_err(|_| invalid("invalid MCP tool scope"))?;
        let old =
            store::get::<w::Connection>(&mut db, "connection", connection_id.as_str()).await?;
        let changed_receiver = old.as_ref().is_some_and(|old| {
            crate::model::receiver(old)
                .ok()
                .zip(crate::model::receiver(&value).ok())
                .is_none_or(|(before, after)| before != after)
        });
        value.credential_configured = old
            .as_ref()
            .is_some_and(|old| old.credential_configured && !changed_receiver);
        if changed_receiver
            && old
                .as_ref()
                .is_some_and(|old| old.credential_env == value.credential_env)
        {
            value.credential_env = None;
        }
        if !value.options_json.is_empty()
            && !serde_json::from_str::<serde_json::Value>(&value.options_json)
                .is_ok_and(|options| options.is_object())
        {
            return Err(invalid("provider options must be a JSON object"));
        }
        drop(db);
        value.revision = key.revision + 1;
        save(
            &self.db,
            "connection",
            key,
            connection_id.as_str(),
            &hash,
            value,
        )
        .await
    }

    async fn put_policy(&mut self, request: w::PolicyChange) -> Result<w::Policy> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("put_policy", &request))?;
        let mut value = request
            .policy
            .into_option()
            .ok_or_else(|| invalid("policy required"))?;
        if value.text.len() > 12_000 || value.always_confirm.len() > 100 {
            return Err(invalid("policy exceeds size limit"));
        }
        value.revision = key.revision + 1;
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        drop(db);
        let saved = save(&self.db, "policy", key, "owner", &hash, value).await?;
        if !self.approvals.is_empty() {
            self.approvals.clear();
            self.announce_snapshot().await?;
        }
        Ok(saved)
    }

    async fn set_credential(&mut self, request: w::CredentialChange) -> Result<w::Empty> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&(
            "set_credential",
            &request.connection_id,
            request.secret.is_some(),
            key.revision,
        ))?;
        let mut connection = self.connection(&request.connection_id).await?;
        let mut tx = self.db.begin().await?;
        if let Some(saved) = store::replay(&mut tx, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        if connection.revision != key.revision {
            return Err(conflict("provider changed"));
        }
        if connection.credential_env.is_some() || connection.kind == w::ProviderKind::McpStdio {
            return Err(invalid(
                "this provider does not accept a keychain credential",
            ));
        }
        if request
            .secret
            .as_ref()
            .is_some_and(|secret| secret.is_empty() || secret.len() > 4096)
        {
            return Err(invalid("invalid credential length"));
        }
        let previous =
            sqlx::query_scalar::<_, String>("SELECT slot FROM credentials WHERE connection_id=?")
                .bind(&connection.id)
                .fetch_optional(&mut *tx)
                .await?;
        connection.credential_configured = request.secret.is_some();
        let slot = id();
        if let Some(secret) = request.secret {
            let destination = crate::model::receiver(&connection)?;
            self.models
                .write_credential(slot.clone(), Some(secret))
                .await?;
            sqlx::query("INSERT INTO credentials VALUES(?,?,?) ON CONFLICT(connection_id) DO UPDATE SET destination=excluded.destination,slot=excluded.slot")
                .bind(&connection.id).bind(destination).bind(&slot).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM credentials WHERE connection_id=?")
                .bind(&connection.id)
                .execute(&mut *tx)
                .await?;
        }
        connection.revision += 1;
        store::put(
            &mut tx,
            "connection",
            &connection.id,
            key.revision,
            &connection,
        )
        .await?;
        let response = w::Empty::default();
        store::acknowledge(&mut tx, &key.request_key, &hash, &response).await?;
        tx.commit().await?;
        if let Some(previous) = previous {
            let models = self.models.clone();
            self.jobs.spawn(async move {
                if models.write_credential(previous, None).await.is_err() {
                    tracing::warn!("retired credential could not be removed from keychain");
                }
            });
        }
        Ok(response)
    }

    async fn delete_record(
        &mut self,
        kind: &'static str,
        request: w::DeleteRequest,
    ) -> Result<w::Empty> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("delete", kind, &request))?;
        let mut tx = self.db.begin().await?;
        if let Some(saved) = store::replay(&mut tx, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        if kind == "character"
            && store::list::<w::Companion>(&mut tx, "companion")
                .await?
                .iter()
                .any(|value| value.character_id == request.id)
        {
            return Err(conflict("character is used by a companion"));
        }
        if kind == "companion"
            && (self.turns.contains_key(&request.id)
                || self
                    .work
                    .values()
                    .any(|work| work.session.companion_id == request.id))
        {
            return Err(conflict("companion has an active turn"));
        }
        store::delete(&mut tx, kind, &request.id, key.revision).await?;
        if kind == "companion" {
            sqlx::query("DELETE FROM schedules WHERE companion_id=?")
                .bind(&request.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM records WHERE kind='activation' AND id=?")
                .bind(&request.id)
                .execute(&mut *tx)
                .await?;
        }
        let retired = if kind == "connection" {
            sqlx::query_scalar::<_, String>(
                "DELETE FROM credentials WHERE connection_id=? RETURNING slot",
            )
            .bind(&request.id)
            .fetch_optional(&mut *tx)
            .await?
        } else {
            None
        };
        let response = w::Empty::default();
        store::acknowledge(&mut tx, &key.request_key, &hash, &response).await?;
        tx.commit().await?;
        if let Some(slot) = retired {
            let models = self.models.clone();
            self.jobs.spawn(async move {
                if models.write_credential(slot, None).await.is_err() {
                    tracing::warn!("retired credential could not be removed from keychain");
                }
            });
        }
        if kind == "companion" {
            self.stop_speech(&request.id);
            self.activations.remove(request.id.as_str());
            self.observations.remove(&request.id);
        }
        if matches!(kind, "character" | "companion") {
            self.announce_snapshot().await?;
        }
        Ok(response)
    }
}

pub(crate) fn validate_profile(profile: &w::Profile) -> Result<()> {
    if profile.name.trim().is_empty() || serde_json::to_vec(profile)?.len() > 16_000 {
        return Err(invalid("profile must have a name and fit within 16 KiB"));
    }
    Ok(())
}
