//! Clients, their declared capabilities, activation generations and bound
//! device operations.
use super::{Result, Store, enum_column, json_column, now, optional_json_column, to_json};
use crate::host::client::{Activation, CapabilitySpec, Client, DeviceOperation, DeviceStatus};
use crate::host::error::{Failure, HostError};
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

const ACTIVATION_COLUMNS: &str = "companion_id, client_id, revision";

fn row_to_activation(row: &Row<'_>) -> rusqlite::Result<Activation> {
    Ok(Activation {
        companion_id: row.get(0)?,
        client_id: row.get(1)?,
        revision: row.get::<_, u64>(2)?,
    })
}

/// Everything a client declares on registration, with the revision it saw.
#[derive(Clone, Copy)]
pub(crate) struct ClientDeclaration<'a> {
    pub(crate) id: &'a str,
    pub(crate) name: &'a str,
    pub(crate) platform: &'a str,
    pub(crate) capabilities: &'a [String],

    pub(crate) specs: &'a [CapabilitySpec],
    pub(crate) expected_revision: u64,
}

impl Store {
    pub(crate) fn clients(&self) -> Result<Vec<Client>> {
        self.fetch_all(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients ORDER BY last_seen DESC, id"),
            [],
            row_to_client,
        )
    }

    pub(crate) fn client(&self, id: &str) -> Result<Option<Client>> {
        self.fetch(
            &format!("SELECT {CLIENT_COLUMNS} FROM clients WHERE id = ?1"),
            [id],
            row_to_client,
        )
    }

    /// Registers or re-registers a client, persisting exactly the grants the
    /// host's policy decided. `expected_revision` must match what the client
    /// last saw; the database never derives a grant from a declaration.
    pub(crate) fn register_client_with_grants(
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
        super::validate_id(id)?;
        let existing = self.client(id)?;
        let current = existing.as_ref().map_or(0, |client| client.revision);
        if expected_revision != current {
            return Err(HostError::conflict(
                "client changed since it was registered",
            ));
        }
        let revision = current + 1;
        let changed = self.conn()?.query_row(
            &format!(
                "INSERT INTO clients (id, name, platform, capabilities, granted_capabilities, \
                 capability_specs, revision, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET name = excluded.name, platform = excluded.platform, \
                 capabilities = excluded.capabilities, granted_capabilities = excluded.granted_capabilities, \
                 capability_specs = excluded.capability_specs, revision = excluded.revision, \
                 last_seen = excluded.last_seen WHERE clients.revision = ?9 \
                 RETURNING {CLIENT_COLUMNS}"
            ),
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
            .ok_or_else(|| HostError::conflict("client changed since it was registered"))
    }

    /// Test-only shorthand for a client that is granted nothing. The host
    /// always passes its policy decision through
    /// [`register_client_with_grants`](Self::register_client_with_grants).
    #[cfg(test)]
    pub(crate) fn register_client(&self, declaration: &ClientDeclaration<'_>) -> Result<Client> {
        self.register_client_with_grants(declaration, &[])
    }

    /// Replaces the declaration of a client and persists exactly the grants
    /// the host decided.
    pub(crate) fn set_capabilities(
        &self,
        id: &str,
        capabilities: &[String],
        granted: &[String],
        specs: &[CapabilitySpec],
        revision: u64,
    ) -> Result<Client> {
        let existing = self
            .client(id)?
            .ok_or_else(|| HostError::not_found("client not found"))?;
        if revision != existing.revision {
            return Err(HostError::conflict("client changed since it was read"));
        }
        let next = existing.revision + 1;
        let changed = self.conn()?.query_row(
            &format!(
                "UPDATE clients SET capabilities = ?2, granted_capabilities = ?3, \
                 capability_specs = ?4, revision = ?5, last_seen = ?6 \
                 WHERE id = ?1 AND revision = ?7 RETURNING {CLIENT_COLUMNS}"
            ),
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
            .ok_or_else(|| HostError::conflict("client changed since it was read"))
    }

    pub(crate) fn activations(&self) -> Result<Vec<Activation>> {
        self.fetch_all(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations ORDER BY companion_id"),
            [],
            row_to_activation,
        )
    }

    pub(crate) fn activation(&self, companion: &str) -> Result<Option<Activation>> {
        self.fetch(
            &format!("SELECT {ACTIVATION_COLUMNS} FROM activations WHERE companion_id = ?1"),
            [companion],
            row_to_activation,
        )
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
        self.fetch(
            &format!(
                "INSERT INTO activations (companion_id, client_id, revision, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(companion_id) DO UPDATE SET client_id = excluded.client_id, \
                 revision = excluded.revision, updated_at = excluded.updated_at \
                 WHERE activations.revision = ?5 RETURNING {ACTIVATION_COLUMNS}"
            ),
            params![companion, client, next, now(), expected],
            row_to_activation,
        )?
        .ok_or_else(|| HostError::conflict("activation changed since it was read"))
    }

    /// The client generation an agent's calls must target: the active client
    /// for a companion, or the client the run's work session is bound to.
    pub(crate) fn target(
        &self,
        companion: &str,
        work: Option<&str>,
    ) -> Result<Option<(String, u64)>> {
        if let Some(work) = work {
            let session = self
                .work_session(work)?
                .ok_or_else(|| HostError::not_found("work session not found"))?;
            return Ok(session
                .client_id
                .map(|client| (client, session.activation_revision)));
        }
        Ok(self.activation(companion)?.and_then(|activation| {
            activation
                .client_id
                .map(|client| (client, activation.revision))
        }))
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
        self.fetch(
            &format!("SELECT {OPERATION_COLUMNS} FROM device_operations WHERE operation_id = ?1"),
            [id],
            row_to_operation,
        )
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
        let updated = self.conn()?.query_row(
            &format!(
                "UPDATE device_operations SET status = ?2, output = ?3, failure = ?4, \
                 updated_at = ?5 WHERE operation_id = ?1 \
                 AND status NOT IN ('completed','failed','uncertain') \
                 RETURNING {OPERATION_COLUMNS}"
            ),
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
    pub(crate) fn outstanding_operations(&self, client: &str) -> Result<Vec<DeviceOperation>> {
        self.fetch_all(
            &format!(
                "SELECT {OPERATION_COLUMNS} FROM device_operations \
                 WHERE client_id = ?1 AND status IN ('pending','dispatched') \
                 ORDER BY created_at"
            ),
            [client],
            row_to_operation,
        )
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

const OPERATION_COLUMNS: &str = "operation_id, client_id, activation_revision, capability, input, \
     work_id, status, output, failure, created_at, updated_at";

fn row_to_operation(row: &Row<'_>) -> rusqlite::Result<DeviceOperation> {
    let status = enum_column(row, "status", "unknown device status", DeviceStatus::parse)?;
    Ok(DeviceOperation {
        id: row.get("operation_id")?,
        client_id: row.get("client_id")?,
        activation_revision: row.get::<_, u64>("activation_revision")?,
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

impl Store {
    /// The activation a client currently holds, with its companion and
    /// generation.
    pub(crate) fn activation_for_client(&self, client: &str) -> Result<Option<(String, u64)>> {
        Ok(self
            .fetch(
                &format!(
                    "SELECT {ACTIVATION_COLUMNS} FROM activations \
                     WHERE client_id = ?1 ORDER BY companion_id"
                ),
                [client],
                row_to_activation,
            )?
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
            approval: crate::host::client::CapabilityApproval::Auto,
            ..Default::default()
        }
    }

    #[test]
    fn registration_persists_exactly_the_grants_the_host_decided() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
        let declaration = ClientDeclaration {
            id: "c1",
            name: "desk",
            platform: "linux",
            capabilities: &["body.motion".into()],
            specs: &[spec("body.motion", "body_motion")],
            expected_revision: 0,
        };
        let client = store
            .register_client_with_grants(&declaration, &["body.motion".to_owned()])
            .unwrap();
        assert_eq!(client.revision, 1);
        assert_eq!(client.granted_capabilities, ["body.motion"]);
        // A stale revision conflicts.
        assert!(store.register_client(&declaration).is_err());
        // The next registration stores the host's new decision, not the one
        // the declaration would imply.
        let client = store
            .register_client_with_grants(
                &ClientDeclaration {
                    expected_revision: client.revision,
                    ..declaration
                },
                &[],
            )
            .unwrap();
        assert_eq!(client.capabilities, ["body.motion"]);
        assert!(client.granted_capabilities.is_empty());
        assert_eq!(client.revision, 2);
        // A capability update persists exactly the grants it is given: a
        // previously granted name does not survive a decision that drops it.
        let specs = [
            spec("body.motion", "body_motion"),
            spec("device.files", "device_files"),
        ];
        let client = store
            .set_capabilities(
                "c1",
                &["body.motion".into(), "device.files".into()],
                &["device.files".into()],
                &specs,
                client.revision,
            )
            .unwrap();
        assert_eq!(client.granted_capabilities, ["device.files"]);
        assert_eq!(client.revision, 3);
        let client = store
            .set_capabilities(
                "c1",
                &["body.motion".into(), "device.files".into()],
                &["body.motion".into()],
                &specs,
                client.revision,
            )
            .unwrap();
        assert_eq!(client.granted_capabilities, ["body.motion"]);
        assert_eq!(client.revision, 4);
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
