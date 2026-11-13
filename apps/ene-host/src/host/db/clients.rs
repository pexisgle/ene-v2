//! Clients, their declared capabilities, activation generations and bound
//! device operations.
use super::{Result, Store, now, to_json};
use crate::host::client::{Activation, CapabilitySpec, Client, DeviceOperation, DeviceStatus};
use crate::host::error::{Failure, HostError};
use rusqlite::{OptionalExtension, Row, params};
use std::collections::HashSet;

fn row_to_client(row: &Row<'_>) -> rusqlite::Result<Client> {
    let capabilities: String = row.get("capabilities")?;
    let granted: String = row.get("granted_capabilities")?;
    let specs: String = row.get("capability_specs")?;
    let corrupt = |text: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            text.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("stored client JSON is corrupt")),
        )
    };
    Ok(Client {
        id: row.get("id")?,
        name: row.get("name")?,
        platform: row.get("platform")?,
        capabilities: serde_json::from_str(&capabilities).map_err(|_| corrupt(&capabilities))?,
        granted_capabilities: serde_json::from_str(&granted).map_err(|_| corrupt(&granted))?,
        capability_specs: serde_json::from_str(&specs).map_err(|_| corrupt(&specs))?,
        revision: row.get::<_, u64>("revision")?,
        last_seen: row.get("last_seen")?,
    })
}

const CLIENT_COLUMNS: &str = "id, name, platform, capabilities, granted_capabilities, \
     capability_specs, revision, last_seen";

/// Everything a client declares on registration, with the revision it saw.
#[derive(Clone, Copy)]
pub(crate) struct ClientDeclaration<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) platform: &'a str,
    pub(crate) capabilities: &'a [String],
    pub(crate) granted_capabilities: &'a [String],
    pub(crate) specs: &'a [CapabilitySpec],
    pub(crate) expected_revision: u64,
}

impl Store {
    pub(crate) fn clients(&self) -> Result<Vec<Client>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {CLIENT_COLUMNS} FROM clients ORDER BY last_seen DESC, id"
        ))?;
        let rows = statement.query_map([], row_to_client)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn client(&self, id: &str) -> Result<Option<Client>> {
        let connection = self.conn()?;
        let client = connection
            .query_row(
                &format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE id = ?1"),
                [id],
                row_to_client,
            )
            .optional()?;
        Ok(client)
    }

    /// Registers or re-registers a client. `expected_revision` must match what
    /// the client last saw; the host never accepts a client's own grants.
    pub(crate) fn register_client(&self, declaration: &ClientDeclaration<'_>) -> Result<Client> {
        let ClientDeclaration {
            id,
            name,
            platform,
            capabilities,
            granted_capabilities,
            specs,
            expected_revision,
        } = *declaration;
        super::validate_id(id)?;
        let existing = self.client(id)?;
        let current = existing.as_ref().map_or(0, |client| client.revision);
        if expected_revision != current {
            return Err(HostError::conflict(
                "client changed since it was registered",
            ));
        }
        let revision = current + 1;
        // The host grants what it has verified; the declaration itself never
        // grants anything and a client cannot extend its own grants.
        let mut granted: Vec<String> = existing
            .as_ref()
            .map(|client| client.granted_capabilities.clone())
            .unwrap_or_default();
        granted.retain(|capability| capabilities.iter().any(|name| name == capability));
        if granted_capabilities.is_empty() {
            for capability in capabilities {
                if !granted.iter().any(|name| name == capability) {
                    granted.push(capability.clone());
                }
            }
        }
        self.conn()?.execute(
            "INSERT INTO clients (id, name, platform, capabilities, granted_capabilities, \
             capability_specs, revision, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
             capabilities = excluded.capabilities, granted_capabilities = excluded.granted_capabilities, \
             capability_specs = excluded.capability_specs, revision = excluded.revision, \
             last_seen = excluded.last_seen",
            params![
                id,
                name,
                platform,
                to_json(&capabilities.to_vec())?,
                to_json(&granted)?,
                to_json(&specs.to_vec())?,
                revision,
                now(),
            ],
        )?;
        self.client(id)?
            .ok_or_else(|| HostError::storage("client registration did not persist"))
    }

    /// Replaces the declaration of a client; grants that no longer have a
    /// declaration are dropped.
    pub(crate) fn set_capabilities(
        &self,
        id: &str,
        capabilities: &[String],
        specs: &[CapabilitySpec],
        revision: u64,
    ) -> Result<Client> {
        let existing = self
            .client(id)?
            .ok_or_else(|| HostError::not_found("client not found"))?;
        if revision != existing.revision {
            return Err(HostError::conflict("client changed since it was read"));
        }
        let declared: HashSet<&str> = capabilities.iter().map(String::as_str).collect();
        let mut granted: Vec<String> = existing
            .granted_capabilities
            .iter()
            .filter(|capability| declared.contains(capability.as_str()))
            .cloned()
            .collect();
        for capability in capabilities {
            if !granted.iter().any(|name| name == capability) {
                granted.push(capability.clone());
            }
        }
        let next = existing.revision + 1;
        self.conn()?.execute(
            "UPDATE clients SET capabilities = ?2, granted_capabilities = ?3, \
             capability_specs = ?4, revision = ?5, last_seen = ?6 WHERE id = ?1",
            params![
                id,
                to_json(&capabilities.to_vec())?,
                to_json(&granted)?,
                to_json(&specs.to_vec())?,
                next,
                now(),
            ],
        )?;
        self.client(id)?
            .ok_or_else(|| HostError::storage("capability update did not persist"))
    }

    pub(crate) fn activations(&self) -> Result<Vec<Activation>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT companion_id, client_id, revision FROM activations ORDER BY companion_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(Activation {
                companion_id: row.get(0)?,
                client_id: row.get(1)?,
                revision: row.get::<_, u64>(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn activation(&self, companion: &str) -> Result<Option<Activation>> {
        let connection = self.conn()?;
        let activation = connection
            .query_row(
                "SELECT companion_id, client_id, revision FROM activations WHERE companion_id = ?1",
                [companion],
                |row| {
                    Ok(Activation {
                        companion_id: row.get(0)?,
                        client_id: row.get(1)?,
                        revision: row.get::<_, u64>(2)?,
                    })
                },
            )
            .optional()?;
        Ok(activation)
    }

    /// Moves or releases the active client and returns the new generation.
    /// `expected` must be the revision the caller last saw (0 for a companion
    /// that was never activated).
    pub(crate) fn activate(
        &self,
        companion: &str,
        client: Option<&str>,
        expected: u64,
    ) -> Result<Activation> {
        let current = self.activation(companion)?;
        let revision = current.as_ref().map_or(0, |activation| activation.revision);
        if expected != revision {
            return Err(HostError::conflict("activation changed since it was read"));
        }
        let next = revision + 1;
        self.conn()?.execute(
            "INSERT INTO activations (companion_id, client_id, revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(companion_id) DO UPDATE SET client_id = excluded.client_id, \
             revision = excluded.revision, updated_at = excluded.updated_at",
            params![companion, client, next, now()],
        )?;
        Ok(Activation {
            companion_id: companion.to_owned(),
            client_id: client.map(ToOwned::to_owned),
            revision: next,
        })
    }

    pub(crate) fn insert_operation(&self, operation: &DeviceOperation) -> Result<()> {
        let stamp = now();
        self.conn()?.execute(
            "INSERT INTO device_operations (operation_id, client_id, activation_revision, \
             capability, input, work_id, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                operation.id,
                operation.client_id,
                operation.activation_revision,
                operation.capability,
                to_json(&operation.input)?,
                operation.work_id,
                operation.status.as_str(),
                stamp,
                stamp,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn operation(&self, id: &str) -> Result<Option<DeviceOperation>> {
        let connection = self.conn()?;
        let operation = connection
            .query_row(
                "SELECT operation_id, client_id, activation_revision, capability, input, \
                 work_id, status, output, failure, created_at, updated_at \
                 FROM device_operations WHERE operation_id = ?1",
                [id],
                row_to_operation,
            )
            .optional()?;
        Ok(operation)
    }

    pub(crate) fn set_operation_status(&self, id: &str, status: DeviceStatus) -> Result<()> {
        self.conn()?.execute(
            "UPDATE device_operations SET status = ?2, updated_at = ?3 \
             WHERE operation_id = ?1",
            params![id, status.as_str(), now()],
        )?;
        Ok(())
    }

    /// Records a client's terminal result. A result for an operation that is
    /// already terminal is ignored, so a late report cannot flip state.
    pub(crate) fn complete_operation(
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
        self.conn()?.execute(
            "UPDATE device_operations SET status = ?2, output = ?3, failure = ?4, \
             updated_at = ?5 WHERE operation_id = ?1 AND status NOT IN ('completed','failed','uncertain')",
            params![
                id,
                status.as_str(),
                output.map(to_json).transpose()?,
                failure.map(to_json).transpose()?,
                now(),
            ],
        )?;
        self.operation(id)
    }

    /// Operations a client may still receive after reconnecting: pending, or
    /// dispatched but never answered.
    pub(crate) fn outstanding_operations(&self, client: &str) -> Result<Vec<DeviceOperation>> {
        let connection = self.conn()?;
        let mut statement = connection.prepare(
            "SELECT operation_id, client_id, activation_revision, capability, input, work_id, \
             status, output, failure, created_at, updated_at FROM device_operations \
             WHERE client_id = ?1 AND status IN ('pending','dispatched') \
             ORDER BY created_at",
        )?;
        let rows = statement.query_map([client], row_to_operation)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// A generation ended: every open operation bound to it fails as
    /// `client_moved` and is never re-dispatched anywhere.
    pub(crate) fn fail_operations_of(&self, client: &str, revision: u64) -> Result<usize> {
        let failure = Failure::new("client_moved", "the active client moved");
        let changed = self.conn()?.execute(
            "UPDATE device_operations SET status = 'failed', failure = ?3, updated_at = ?4 \
             WHERE client_id = ?1 AND activation_revision = ?2 \
             AND status IN ('pending','dispatched')",
            params![client, revision, to_json(&failure)?, now()],
        )?;
        Ok(changed)
    }

    /// Startup: dispatched operations whose result never arrived are no
    /// longer trusted. Pending ones wait for the client to reconnect.
    pub(crate) fn mark_unanswered_operations_uncertain(&self) -> Result<usize> {
        let changed = self.conn()?.execute(
            "UPDATE device_operations SET status = 'uncertain', updated_at = ?1 \
             WHERE status = 'dispatched'",
            [now()],
        )?;
        Ok(changed)
    }
}

fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<DeviceOperation> {
    let status: String = row.get("status")?;
    let status = DeviceStatus::parse(&status).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            status.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("unknown device status")),
        )
    })?;
    let input: String = row.get("input")?;
    let output: Option<String> = row.get("output")?;
    let failure: Option<String> = row.get("failure")?;
    let corrupt = |text: &str| {
        rusqlite::Error::FromSqlConversionFailure(
            text.len(),
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("stored operation JSON is corrupt")),
        )
    };
    Ok(DeviceOperation {
        id: row.get("operation_id")?,
        client_id: row.get("client_id")?,
        activation_revision: row.get::<_, u64>("activation_revision")?,
        capability: row.get("capability")?,
        input: serde_json::from_str(&input).map_err(|_| corrupt(&input))?,
        work_id: row.get("work_id")?,
        status,
        output: output
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| corrupt(output.as_deref().unwrap_or_default()))?,
        failure: failure
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| corrupt(failure.as_deref().unwrap_or_default()))?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

impl Store {
    /// The activation a client currently holds, with its companion and
    /// generation.
    pub(crate) fn activation_for_client(&self, client: &str) -> Result<Option<(String, u64)>> {
        Ok(self
            .activations()?
            .into_iter()
            .find(|activation| activation.client_id.as_deref() == Some(client))
            .map(|activation| (activation.companion_id, activation.revision)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, tool: &str) -> CapabilitySpec {
        CapabilitySpec {
            name: name.into(),
            tool_name: tool.into(),
            description: String::new(),
            input_schema: "{}".into(),
            presets: Vec::new(),
            approval: crate::host::client::CapabilityApproval::Auto,
            approval_target: String::new(),
            output_schema: String::new(),
        }
    }

    #[test]
    fn registration_grants_only_declared_capabilities() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let client = store
            .register_client(&ClientDeclaration {
                id: "c1",
                name: "desk",
                platform: "linux",
                capabilities: &["body.motion".into()],
                granted_capabilities: &[],
                specs: &[spec("body.motion", "body_motion")],
                expected_revision: 0,
            })
            .unwrap();
        assert_eq!(client.revision, 1);
        assert!(client.is_granted("body.motion"));
        // A stale revision conflicts.
        assert!(
            store
                .register_client(&ClientDeclaration {
                    id: "c1",
                    name: "desk",
                    platform: "linux",
                    capabilities: &[],
                    granted_capabilities: &[],
                    specs: &[],
                    expected_revision: 0,
                })
                .is_err()
        );
        // The client cannot grant itself anything that it does not declare.
        let client = store
            .set_capabilities("c1", &[], &[], client.revision)
            .unwrap();
        assert!(client.granted_capabilities.is_empty());
        assert_eq!(client.revision, 2);
    }

    #[test]
    fn activations_move_forward_and_require_the_expected_revision() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        assert!(store.activation("ene").unwrap().is_none());
        let first = store.activate("ene", Some("c1"), 0).unwrap();
        assert_eq!(
            (first.revision, first.client_id.as_deref()),
            (1, Some("c1"))
        );
        assert!(store.activate("ene", Some("c2"), 0).is_err());
        let second = store.activate("ene", Some("c2"), 1).unwrap();
        assert_eq!(second.revision, 2);
        let released = store.activate("ene", None, 2).unwrap();
        assert_eq!((released.revision, released.client_id), (3, None));
        assert_eq!(store.activations().unwrap().len(), 1);
    }

    #[test]
    fn operations_are_idempotent_and_generation_bound() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let operation = DeviceOperation {
            id: "op1".into(),
            client_id: "c1".into(),
            activation_revision: 1,
            capability: "body.motion".into(),
            input: serde_json::json!({"motion": "nod"}),
            work_id: None,
            status: DeviceStatus::Pending,
            output: None,
            failure: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        store.insert_operation(&operation).unwrap();
        assert_eq!(store.outstanding_operations("c1").unwrap().len(), 1);
        store
            .set_operation_status("op1", DeviceStatus::Dispatched)
            .unwrap();
        let completed = store
            .complete_operation("op1", true, Some(&serde_json::json!({"ok": true})), None)
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, DeviceStatus::Completed);
        // A second result cannot flip a terminal operation.
        let again = store
            .complete_operation("op1", false, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(again.status, DeviceStatus::Completed);
        // A moved generation fails open operations and nothing is re-sent.
        let other = DeviceOperation {
            id: "op2".into(),
            activation_revision: 1,
            ..operation.clone()
        };
        store.insert_operation(&other).unwrap();
        assert_eq!(store.fail_operations_of("c1", 1).unwrap(), 1);
        assert!(store.outstanding_operations("c1").unwrap().is_empty());
    }
}
