//! Clients, their declared capabilities, the active client per companion and
//! bound device operations.

mod device_row;

use crate::row::{FromRow, row_fields};
use crate::{
    Store, StoreError,
    error::Result,
    id_column, json_column, to_json,
    util::{now, validate_id},
};
use device_row::OPERATION_COLUMNS;
use ene_domain::{
    Activation, Client, CompanionId, DeviceOperation, DeviceScope, DeviceStatus, Failure,
    SubscriptionId,
};
use ene_policy::CapabilitySpec;
use rusqlite::{OptionalExtension, Row, params};

const CLIENT_COLUMNS: &str =
    "id, name, platform, capabilities, capability_specs, revision, last_seen";
const INVALID_CLIENT_JSON: &str = "stored client JSON is corrupt";

impl FromRow for Client {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            platform: row.get("platform")?,
            capabilities: json_column(row, "capabilities", INVALID_CLIENT_JSON)?,
            capability_specs: json_column(row, "capability_specs", INVALID_CLIENT_JSON)?,
            revision: row.get("revision")?,
            last_seen: row.get("last_seen")?,
            granted_capabilities: Vec::new(),
        })
    }
}

row_fields!(
    Activation,
    ACTIVATION_COLUMNS {
        companion_id => id_column("stored companion id is invalid"),
        client_id,
        generation,
    }
);

/// Everything a client declares on registration, with the revision it saw.
#[derive(Clone, Copy, Debug)]
pub struct ClientDeclaration<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub platform: &'a str,
    pub capabilities: &'a [String],
    pub specs: &'a [CapabilitySpec],
    pub expected_revision: u64,
}

/// Repository managing `clients`, `activations`, and `device_operations`.
#[derive(Clone, Copy, Debug)]
pub struct ClientRepo<'a>(&'a Store);

impl<'a> ClientRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        let mut clients = self.0.fetch_all::<Client>(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients ORDER BY last_seen DESC, id"),
            [],
            Client::from_row,
        )?;
        self.project_grants(&mut clients)?;
        Ok(clients)
    }

    pub fn client(&self, id: &str) -> Result<Option<Client>> {
        let mut client = self.0.fetch(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE id = ?1"),
            [id],
            Client::from_row,
        )?;
        self.project_grants(client.as_mut_slice())?;
        Ok(client)
    }

    fn project_grants(&self, clients: &mut [Client]) -> Result<()> {
        if !clients.is_empty() {
            let rules = self.0.policy_repo().capability_rules()?;
            for client in clients {
                client.granted_capabilities = self.0.grants.granted(&client.capabilities, &rules);
            }
        }
        Ok(())
    }

    fn client_revision(&self, id: &str) -> Result<Option<u64>> {
        self.0
            .fetch("SELECT revision FROM clients WHERE id = ?1", [id], |row| {
                row.get(0)
            })
    }

    /// Registers or re-registers a client against the revision it last saw.
    pub fn register_client(&self, declaration: &ClientDeclaration<'_>) -> Result<Client> {
        let ClientDeclaration {
            id,
            name,
            platform,
            capabilities,
            specs,
            expected_revision,
        } = *declaration;
        validate_id(id)?;
        let current = self.client_revision(id)?.unwrap_or_default();
        if expected_revision != current {
            return Err(StoreError::Conflict(
                "client changed since it was registered".into(),
            ));
        }
        let revision = current + 1;
        let changed = self
            .0
            .conn()
            .prepare_cached(&format!(
                "INSERT INTO clients (id, name, platform, capabilities, capability_specs, \
                 revision, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
                 capabilities = excluded.capabilities, \
                 capability_specs = excluded.capability_specs, revision = excluded.revision, \
                 last_seen = excluded.last_seen WHERE clients.revision = ?8 \
                 RETURNING {CLIENT_COLUMNS}"
            ))?
            .query_row(
                params![
                    id,
                    name,
                    platform,
                    to_json(capabilities)?,
                    to_json(specs)?,
                    revision,
                    now(),
                    current,
                ],
                Client::from_row,
            );
        let mut client = changed
            .optional()?
            .ok_or_else(|| StoreError::Conflict("client changed since it was registered".into()))?;
        self.project_grants(std::slice::from_mut(&mut client))?;
        Ok(client)
    }

    /// Replaces a client's declaration against the revision it last saw.
    pub fn set_capabilities(
        &self,
        id: &str,
        capabilities: &[String],
        specs: &[CapabilitySpec],
        revision: u64,
    ) -> Result<Client> {
        let current = self
            .client_revision(id)?
            .ok_or_else(|| StoreError::NotFound("client not found".into()))?;
        if revision != current {
            return Err(StoreError::Conflict(
                "client changed since it was read".into(),
            ));
        }
        let next = current + 1;
        let changed = self
            .0
            .conn()
            .prepare_cached(&format!(
                "UPDATE clients SET capabilities = ?2, capability_specs = ?3, \
                 revision = ?4, last_seen = ?5 \
                 WHERE id = ?1 AND revision = ?6 RETURNING {CLIENT_COLUMNS}"
            ))?
            .query_row(
                params![
                    id,
                    to_json(capabilities)?,
                    to_json(specs)?,
                    next,
                    now(),
                    current,
                ],
                Client::from_row,
            );
        let mut client = changed
            .optional()?
            .ok_or_else(|| StoreError::Conflict("client changed since it was read".into()))?;
        self.project_grants(std::slice::from_mut(&mut client))?;
        Ok(client)
    }

    pub fn activations(&self) -> Result<Vec<Activation>> {
        self.0.fetch_all(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations ORDER BY companion_id"),
            [],
            Activation::from_row,
        )
    }

    pub fn activation(&self, companion: &str) -> Result<Option<Activation>> {
        self.0.fetch(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations WHERE companion_id = ?1"),
            [companion],
            Activation::from_row,
        )
    }

    /// Moves or releases the active client. The latest write wins: a companion
    /// gets a new generation only when the selected client actually changes.
    pub fn activate(&self, companion: &str, client: Option<&str>) -> Result<Activation> {
        self.0
            .fetch(
                &format!(
                    "INSERT INTO activations (companion_id, client_id, updated_at) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(companion_id) DO UPDATE SET client_id = excluded.client_id, \
                     generation = generation + (activations.client_id IS NOT excluded.client_id), \
                     updated_at = excluded.updated_at RETURNING {ACTIVATION_COLUMNS}"
                ),
                params![companion, client, now()],
                Activation::from_row,
            )?
            .ok_or_else(|| StoreError::Storage("the activation did not persist".into()))
    }

    /// The client an agent's calls are dispatched to: the client the run's work
    /// session is bound to, else the companion's active client.
    pub fn target(&self, companion: &str, work: Option<&str>) -> Result<Option<String>> {
        if let Some(work) = work {
            let session = self
                .0
                .work_repo()
                .work_session(work)?
                .ok_or_else(|| StoreError::NotFound("work session not found".into()))?;
            return Ok(session.client_id);
        }
        Ok(self
            .activation(companion)?
            .and_then(|activation| activation.client_id))
    }

    pub fn insert_operation(&self, operation: &DeviceOperation) -> Result<()> {
        let companion = operation.scope.companion_id().ok_or_else(|| {
            StoreError::Invalid("a new device operation requires a recorded owner".into())
        })?;
        let activation_generation = match &operation.scope {
            DeviceScope::Companion {
                activation_generation,
                ..
            } => Some(activation_generation.ok_or_else(|| {
                StoreError::Invalid(
                    "a new companion operation requires its activation generation".into(),
                )
            })?),
            _ => None,
        };
        let stamp = now();
        self.0
            .conn()
            .prepare_cached(
                "INSERT INTO device_operations (operation_id, client_id, \
                 capability, input, companion_id, work_id, run_id, status, created_at, updated_at, activation_generation) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
            )?
            .execute(params![
                operation.id,
                operation.client_id,
                operation.capability,
                to_json(&operation.input)?,
                companion.as_str(),
                operation.scope.work_id(),
                operation.scope.run_id(),
                operation.status.as_str(),
                stamp,
                activation_generation,
            ])?;
        Ok(())
    }

    pub fn operation(&self, id: &str) -> Result<Option<DeviceOperation>> {
        Ok(self
            .operation_with_subscription(id)?
            .map(|(operation, _)| operation))
    }

    /// Reads an operation and the exact stream selected by its durable claim.
    pub fn operation_with_subscription(
        &self,
        id: &str,
    ) -> Result<Option<(DeviceOperation, Option<String>)>> {
        self.0.fetch(
            &format!("SELECT {OPERATION_COLUMNS} FROM device_operations WHERE operation_id = ?1"),
            [id],
            |row| {
                Ok((
                    DeviceOperation::from_row(row)?,
                    row.get("dispatch_subscription")?,
                ))
            },
        )
    }

    /// Claims a pending operation before it is made visible on a device stream.
    pub fn mark_operation_dispatched(
        &self,
        id: &str,
        subscription: SubscriptionId,
    ) -> Result<bool> {
        Ok(self
            .0
            .conn()
            .prepare_cached(
                "UPDATE device_operations SET status = 'dispatched', dispatch_subscription = ?2, updated_at = ?3 \
             WHERE operation_id = ?1 AND status = 'pending'",
            )?
            .execute(params![id, subscription.to_string(), now()])?
            != 0)
    }

    /// Advances an active operation without overwriting a terminal result.
    pub fn set_operation_status(&self, id: &str, status: DeviceStatus) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(
                "UPDATE device_operations SET status = ?2, updated_at = ?3 \
                 WHERE operation_id = ?1 AND status IN ('pending','dispatched')",
            )?
            .execute(params![id, status.as_str(), now()])?;
        Ok(())
    }

    /// Records a client's terminal result. A result for an operation that is
    /// already terminal is ignored, so a late report cannot flip state.
    pub fn complete_operation(
        &self,
        id: &str,
        outcome: std::result::Result<&serde_json::Value, &Failure>,
    ) -> Result<Option<DeviceOperation>> {
        let (status, output, failure) = match outcome {
            Ok(output) => (DeviceStatus::Completed, Some(to_json(output)?), None),
            Err(failure) => (DeviceStatus::Failed, None, Some(to_json(failure)?)),
        };
        let updated = self
            .0
            .conn()
            .prepare_cached(&format!(
                "UPDATE device_operations SET status = ?2, output = ?3, failure = ?4, \
                 updated_at = ?5 WHERE operation_id = ?1 \
                 AND status NOT IN ('completed','failed','uncertain') \
                 RETURNING {OPERATION_COLUMNS}"
            ))?
            .query_row(
                params![id, status.as_str(), output, failure, now(),],
                DeviceOperation::from_row,
            );
        match updated.optional()? {
            Some(operation) => Ok(Some(operation)),
            None => self.operation(id),
        }
    }

    /// Operations a client may still receive after reconnecting: pending, or
    /// dispatched but never answered.
    pub fn outstanding_operations(&self, client: &str) -> Result<Vec<DeviceOperation>> {
        self.0.fetch_all(
            &format!(
                "SELECT {OPERATION_COLUMNS} FROM device_operations \
                 WHERE client_id = ?1 AND status IN ('pending','dispatched') \
                 ORDER BY created_at"
            ),
            [client],
            DeviceOperation::from_row,
        )
    }

    /// Marks unanswered dispatches uncertain at startup or for one disconnected client.
    pub fn mark_unanswered_operations_uncertain(
        &self,
        client: Option<&str>,
    ) -> Result<Vec<String>> {
        self.0.fetch_all(
            "UPDATE device_operations SET status = 'uncertain', updated_at = ?1
             WHERE status = 'dispatched' AND (?2 IS NULL OR client_id = ?2)
             RETURNING operation_id",
            params![now(), client],
            |row| row.get(0),
        )
    }

    /// The companion a client is currently the active client of, if any.
    pub fn activation_for_client(&self, client: &str) -> Result<Option<String>> {
        self.0.fetch(
            "SELECT companion_id FROM activations WHERE client_id = ?1 ORDER BY companion_id LIMIT 1",
            [client],
            active_companion,
        )
    }

    /// All companions currently resident (active) on `client`.
    pub fn active_companions_for_client(&self, client: &str) -> Result<Vec<String>> {
        self.0.fetch_all(
            "SELECT companion_id FROM activations WHERE client_id = ?1 ORDER BY companion_id",
            [client],
            active_companion,
        )
    }

    /// Restores a client row exactly as it was, for rolling back a declaration
    /// whose tool publication failed.
    pub fn restore_client(&self, client: &Client) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO clients (id, name, platform, capabilities, \
                 capability_specs, revision, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                client.id,
                client.name,
                client.platform,
                to_json(&client.capabilities)?,
                to_json(&client.capability_specs)?,
                client.revision,
                client.last_seen,
            ])?;
        Ok(())
    }

    pub fn delete_client(&self, id: &str) -> Result<()> {
        self.0
            .conn()
            .prepare_cached("DELETE FROM clients WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    /// Restores an activation row exactly, for rolling back a move whose tool
    /// publication failed.
    pub fn restore_activation(&self, activation: &Activation) -> Result<()> {
        self.0
            .conn()
            .prepare_cached(
                "INSERT OR REPLACE INTO activations (companion_id, client_id, updated_at, generation) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                activation.companion_id.as_str(),
                activation.client_id,
                now(),
                activation.generation,
            ])?;
        Ok(())
    }

    pub fn delete_activation(&self, companion: &str) -> Result<()> {
        self.0
            .conn()
            .prepare_cached("DELETE FROM activations WHERE companion_id = ?1")?
            .execute([companion])?;
        Ok(())
    }
}

fn active_companion(row: &Row<'_>) -> rusqlite::Result<String> {
    id_column::<CompanionId>(row, "companion_id", "stored companion id is invalid")
        .map(|id| id.to_string())
}

#[cfg(test)]
#[path = "clients_tests.rs"]
mod tests;
