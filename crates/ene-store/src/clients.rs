//! Clients, their declared capabilities, the active client per companion and
//! bound device operations.

use crate::{
    Store, StoreError, enum_name_column,
    error::Result,
    json_column, optional_json_column, to_json,
    util::{now, validate_id},
};
use ene_domain::{Activation, Client, DeviceOperation, DeviceStatus, Failure};
use ene_policy::CapabilitySpec;
use rusqlite::{OptionalExtension, Row, params};

fn row_to_client(row: &Row<'_>) -> rusqlite::Result<Client> {
    Ok(Client {
        id: row.get("id")?,
        name: row.get("name")?,
        platform: row.get("platform")?,
        capabilities: json_column(row, "capabilities", "stored client JSON is corrupt")?,
        granted_capabilities: json_column(
            row,
            "granted_capabilities",
            "stored client JSON is corrupt",
        )?,
        capability_specs: json_column(row, "capability_specs", "stored client JSON is corrupt")?,
        revision: row.get::<_, u64>("revision")?,
        last_seen: row.get("last_seen")?,
    })
}

const CLIENT_COLUMNS: &str = "id, name, platform, capabilities, granted_capabilities, \
     capability_specs, revision, last_seen";

const ACTIVATION_COLUMNS: &str = "companion_id, client_id";

const OPERATION_COLUMNS: &str = "operation_id, client_id, capability, input, \
     work_id, status, output, failure, created_at, updated_at";

fn row_to_activation(row: &Row<'_>) -> rusqlite::Result<Activation> {
    Ok(Activation {
        companion_id: row.get(0)?,
        client_id: row.get(1)?,
    })
}

fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<DeviceOperation> {
    let status = enum_name_column(row, "status", "unknown device status")?;
    Ok(DeviceOperation {
        id: row.get("operation_id")?,
        client_id: row.get("client_id")?,
        capability: row.get("capability")?,
        input: json_column(row, "input", "stored operation JSON is corrupt")?,
        work_id: row.get("work_id")?,
        status,
        output: optional_json_column(row, "output", "stored operation JSON is corrupt")?,
        failure: optional_json_column(row, "failure", "stored operation JSON is corrupt")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

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
        self.0.fetch_all(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients ORDER BY last_seen DESC, id"),
            [],
            row_to_client,
        )
    }

    pub fn client(&self, id: &str) -> Result<Option<Client>> {
        self.0.fetch(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE id = ?1"),
            [id],
            row_to_client,
        )
    }

    /// Registers or re-registers a client, persisting exactly the grants the
    /// host's policy decided. `expected_revision` must match what the client
    /// last saw; the database never derives a grant from a declaration.
    pub fn register_client_with_grants(
        &self,
        declaration: &ClientDeclaration<'_>,
        granted: &[String],
    ) -> Result<Client> {
        let ClientDeclaration {
            id,
            name,
            platform,
            capabilities,
            specs,
            expected_revision,
        } = *declaration;
        validate_id(id)?;
        let existing = self.client(id)?;
        let current = existing.as_ref().map_or(0, |client| client.revision);
        if expected_revision != current {
            return Err(StoreError::Conflict(
                "client changed since it was registered".into(),
            ));
        }
        let revision = current + 1;
        let changed = self
            .0
            .conn()?
            .prepare_cached(&format!(
                "INSERT INTO clients (id, name, platform, capabilities, granted_capabilities, \
                 capability_specs, revision, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
                 capabilities = excluded.capabilities, granted_capabilities = excluded.granted_capabilities, \
                 capability_specs = excluded.capability_specs, revision = excluded.revision, \
                 last_seen = excluded.last_seen WHERE clients.revision = ?9 \
                 RETURNING {CLIENT_COLUMNS}"
            ))?
            .query_row(
                params![
                    id,
                    name,
                    platform,
                    to_json(&capabilities.to_vec())?,
                    to_json(&granted)?,
                    to_json(&specs.to_vec())?,
                    revision,
                    now(),
                    current,
                ],
                row_to_client,
            );
        changed
            .optional()?
            .ok_or_else(|| StoreError::Conflict("client changed since it was registered".into()))
    }

    /// Test helper for registering a client that is granted nothing.
    /// Production registration passes its policy decision through
    /// [`register_client_with_grants`](Self::register_client_with_grants).
    pub fn register_client(&self, declaration: &ClientDeclaration<'_>) -> Result<Client> {
        self.register_client_with_grants(declaration, &[])
    }

    /// Replaces the declaration of a client and persists exactly the grants
    /// the host decided.
    pub fn set_capabilities(
        &self,
        id: &str,
        capabilities: &[String],
        granted: &[String],
        specs: &[CapabilitySpec],
        revision: u64,
    ) -> Result<Client> {
        let existing = self
            .client(id)?
            .ok_or_else(|| StoreError::NotFound("client not found".into()))?;
        if revision != existing.revision {
            return Err(StoreError::Conflict(
                "client changed since it was read".into(),
            ));
        }
        let next = existing.revision + 1;
        let changed = self
            .0
            .conn()?
            .prepare_cached(&format!(
                "UPDATE clients SET capabilities = ?2, granted_capabilities = ?3, \
                 capability_specs = ?4, revision = ?5, last_seen = ?6 \
                 WHERE id = ?1 AND revision = ?7 RETURNING {CLIENT_COLUMNS}"
            ))?
            .query_row(
                params![
                    id,
                    to_json(&capabilities.to_vec())?,
                    to_json(&granted)?,
                    to_json(&specs.to_vec())?,
                    next,
                    now(),
                    existing.revision,
                ],
                row_to_client,
            );
        changed
            .optional()?
            .ok_or_else(|| StoreError::Conflict("client changed since it was read".into()))
    }

    pub fn activations(&self) -> Result<Vec<Activation>> {
        self.0.fetch_all(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations ORDER BY companion_id"),
            [],
            row_to_activation,
        )
    }

    pub fn activation(&self, companion: &str) -> Result<Option<Activation>> {
        self.0.fetch(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations WHERE companion_id = ?1"),
            [companion],
            row_to_activation,
        )
    }

    /// Moves or releases the active client. The latest write wins: a companion
    /// has exactly one active client row, with no generation to compare.
    pub fn activate(&self, companion: &str, client: Option<&str>) -> Result<Activation> {
        self.0
            .fetch(
                &format!(
                    "INSERT INTO activations (companion_id, client_id, updated_at) \
                     VALUES (?1, ?2, ?3) \
                     ON CONFLICT(companion_id) DO UPDATE SET client_id = excluded.client_id, \
                     updated_at = excluded.updated_at RETURNING {ACTIVATION_COLUMNS}"
                ),
                params![companion, client, now()],
                row_to_activation,
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
        let stamp = now();
        self.0
            .conn()?
            .prepare_cached(
                "INSERT INTO device_operations (operation_id, client_id, \
                 capability, input, work_id, status, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                operation.id,
                operation.client_id,
                operation.capability,
                to_json(&operation.input)?,
                operation.work_id,
                operation.status.as_str(),
                stamp,
                stamp,
            ])?;
        Ok(())
    }

    pub fn operation(&self, id: &str) -> Result<Option<DeviceOperation>> {
        self.0.fetch(
            &format!("SELECT {OPERATION_COLUMNS} FROM device_operations WHERE operation_id = ?1"),
            [id],
            row_to_operation,
        )
    }

    pub fn set_operation_status(&self, id: &str, status: DeviceStatus) -> Result<()> {
        self.0
            .conn()?
            .prepare_cached(
                "UPDATE device_operations SET status = ?2, updated_at = ?3 \
                 WHERE operation_id = ?1",
            )?
            .execute(params![id, status.as_str(), now()])?;
        Ok(())
    }

    /// Records a client's terminal result. A result for an operation that is
    /// already terminal is ignored, so a late report cannot flip state.
    pub fn complete_operation(
        &self,
        id: &str,
        success: bool,
        output: Option<&serde_json::Value>,
        failure: Option<&Failure>,
    ) -> Result<Option<DeviceOperation>> {
        let Some(operation) = self.operation(id)? else {
            return Ok(None);
        };
        if operation.status.is_terminal() {
            return Ok(Some(operation));
        }
        let status = if success {
            DeviceStatus::Completed
        } else {
            DeviceStatus::Failed
        };
        let updated = self
            .0
            .conn()?
            .prepare_cached(&format!(
                "UPDATE device_operations SET status = ?2, output = ?3, failure = ?4, \
                 updated_at = ?5 WHERE operation_id = ?1 \
                 AND status NOT IN ('completed','failed','uncertain') \
                 RETURNING {OPERATION_COLUMNS}"
            ))?
            .query_row(
                params![
                    id,
                    status.as_str(),
                    output.map(to_json).transpose()?,
                    failure.map(to_json).transpose()?,
                    now(),
                ],
                row_to_operation,
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
            row_to_operation,
        )
    }

    /// Startup: dispatched operations whose result never arrived are no
    /// longer trusted. Pending ones wait for the client to reconnect.
    pub fn mark_unanswered_operations_uncertain(&self) -> Result<usize> {
        let changed = self
            .0
            .conn()?
            .prepare_cached(
                "UPDATE device_operations SET status = 'uncertain', updated_at = ?1 \
                 WHERE status = 'dispatched'",
            )?
            .execute([now()])?;
        Ok(changed)
    }

    /// The companion a client is currently the active client of, if any.
    pub fn activation_for_client(&self, client: &str) -> Result<Option<String>> {
        Ok(self
            .active_companions_for_client(client)?
            .into_iter()
            .next())
    }

    /// All companions currently resident (active) on `client`.
    pub fn active_companions_for_client(&self, client: &str) -> Result<Vec<String>> {
        self.0
            .fetch_all(
                &format!(
                    "SELECT {ACTIVATION_COLUMNS} FROM activations \
                     WHERE client_id = ?1 ORDER BY companion_id"
                ),
                [client],
                row_to_activation,
            )
            .map(|items| items.into_iter().map(|a| a.companion_id).collect())
    }

    /// Restores a client row exactly as it was, for rolling back a declaration
    /// whose tool publication failed.
    pub fn restore_client(&self, client: &Client) -> Result<()> {
        self.0
            .conn()?
            .prepare_cached(
                "INSERT OR REPLACE INTO clients (id, name, platform, capabilities, \
                 granted_capabilities, capability_specs, revision, last_seen) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                client.id,
                client.name,
                client.platform,
                to_json(&client.capabilities)?,
                to_json(&client.granted_capabilities)?,
                to_json(&client.capability_specs)?,
                client.revision,
                client.last_seen,
            ])?;
        Ok(())
    }

    pub fn delete_client(&self, id: &str) -> Result<()> {
        self.0
            .conn()?
            .prepare_cached("DELETE FROM clients WHERE id = ?1")?
            .execute([id])?;
        Ok(())
    }

    /// Restores an activation row exactly, for rolling back a move whose tool
    /// publication failed.
    pub fn restore_activation(&self, activation: &Activation) -> Result<()> {
        self.0
            .conn()?
            .prepare_cached(
                "INSERT OR REPLACE INTO activations (companion_id, client_id, updated_at) \
                 VALUES (?1, ?2, ?3)",
            )?
            .execute(params![
                activation.companion_id,
                activation.client_id,
                now(),
            ])?;
        Ok(())
    }

    pub fn delete_activation(&self, companion: &str) -> Result<()> {
        self.0
            .conn()?
            .prepare_cached("DELETE FROM activations WHERE companion_id = ?1")?
            .execute([companion])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_are_idempotent_and_bound_to_one_client() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let repo = store.client_repo();
        let operation = DeviceOperation {
            id: "op1".into(),
            client_id: "c1".into(),
            capability: "body.motion".into(),
            input: serde_json::json!({"motion": "nod"}),
            work_id: None,
            status: DeviceStatus::Pending,
            output: None,
            failure: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        repo.insert_operation(&operation).unwrap();
        assert_eq!(repo.outstanding_operations("c1").unwrap().len(), 1);
        repo.set_operation_status("op1", DeviceStatus::Dispatched)
            .unwrap();
        let completed = repo
            .complete_operation("op1", true, Some(&serde_json::json!({"ok": true})), None)
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, DeviceStatus::Completed);
        let again = repo
            .complete_operation("op1", false, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(again.status, DeviceStatus::Completed);
        assert_eq!(
            repo.operation("op1").unwrap().unwrap().client_id.as_str(),
            "c1"
        );
        let other = DeviceOperation {
            id: "op2".into(),
            client_id: "c2".into(),
            ..operation.clone()
        };
        repo.insert_operation(&other).unwrap();
        assert_eq!(repo.outstanding_operations("c1").unwrap().len(), 0);
        assert_eq!(repo.outstanding_operations("c2").unwrap().len(), 1);
    }
}
