//! Publishes vault references independently of the protected secret values.

use crate::{Result, Store, StoreError, corrupt, json_column, require_found, util::now};
use ene_domain::{ConnectionConfig, ConnectionId, CredentialState};
use rusqlite::{Row, params};
use uuid::Uuid;

/// An immutable slot in the host's vault or current session.
#[derive(Clone, Copy, Debug)]
pub struct VaultSlot {
    /// A fresh address for this registration.
    pub id: Uuid,
    /// Whether the slot survives this host process.
    pub session_only: bool,
}

/// The active slot and settings against which its recipient can be checked.
#[derive(Debug)]
pub struct CredentialBinding {
    /// Published credential address.
    pub slot: VaultSlot,
    /// Current connection configuration.
    pub receiver: ConnectionConfig,
}

/// Nonsecret metadata for active and detached vault entries.
#[derive(Debug)]
pub struct CredentialRepo<'a>(&'a Store);

impl<'a> CredentialRepo<'a> {
    pub(crate) fn new(store: &'a Store) -> Self {
        Self(store)
    }

    /// Records a detached address before any secret can be written there.
    pub fn stage(&self, session_only: bool) -> Result<VaultSlot> {
        let slot = VaultSlot {
            id: Uuid::new_v4(),
            session_only,
        };
        self.0.conn().execute(
            "INSERT INTO credential_slots (id, session_only) VALUES (?1, ?2)",
            params![slot.id.to_string(), session_only],
        )?;
        Ok(slot)
    }

    /// Atomically binds a staged slot and retires the previous registration.
    pub fn publish(
        &self,
        connection: &ConnectionId,
        revision: u64,
        created_revision: u64,
        slot: VaultSlot,
    ) -> Result<()> {
        self.0.write_tx(|tx| {
            let changed = tx.execute(
                "UPDATE connections SET credential_state = ?4, updated_at = ?5, last_check = NULL
                 WHERE id = ?1 AND revision = ?2 AND created_revision = ?3",
                params![
                    connection.as_str(),
                    revision,
                    created_revision,
                    CredentialState::Registered.as_str(),
                    now()
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict(
                    "the credential receiver changed".into(),
                ));
            }
            tx.execute(
                "UPDATE credential_slots SET connection_id = NULL WHERE connection_id = ?1",
                [connection.as_str()],
            )?;
            let changed = tx.execute(
                "UPDATE credential_slots SET connection_id = ?1
                 WHERE id = ?2 AND session_only = ?3 AND connection_id IS NULL",
                params![connection.as_str(), slot.id.to_string(), slot.session_only],
            )?;
            require_found(changed, "staged credential slot not found")
        })
    }

    /// Revokes access by detaching the active slot before any vault operation.
    pub fn revoke(
        &self,
        connection: &ConnectionId,
        revision: u64,
        created_revision: u64,
    ) -> Result<()> {
        self.0.write_tx(|tx| {
            let changed = tx.execute(
                "UPDATE connections SET credential_state = ?4, updated_at = ?5, last_check = NULL
                 WHERE id = ?1 AND revision = ?2 AND created_revision = ?3",
                params![
                    connection.as_str(),
                    revision,
                    created_revision,
                    CredentialState::Absent.as_str(),
                    now()
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict(
                    "the credential receiver changed".into(),
                ));
            }
            tx.execute(
                "UPDATE credential_slots SET connection_id = NULL WHERE connection_id = ?1",
                [connection.as_str()],
            )?;
            Ok(())
        })
    }

    /// Resolves only addresses that have been published for the connection.
    pub fn binding(&self, connection: &str) -> Result<Option<CredentialBinding>> {
        self.0.fetch(
            "SELECT s.id, s.session_only, c.config FROM credential_slots s
             JOIN connections c ON c.id = s.connection_id WHERE c.id = ?1",
            [connection],
            |row| {
                Ok(CredentialBinding {
                    slot: slot(row)?,
                    receiver: json_column(row, "config", "stored credential receiver is corrupt")?,
                })
            },
        )
    }

    /// Detached slots that may still need erasure.
    pub fn detached(&self) -> Result<Vec<VaultSlot>> {
        self.0.fetch_all(
            "SELECT id, session_only FROM credential_slots WHERE connection_id IS NULL",
            [],
            slot,
        )
    }

    /// Confirms erasure of a detached slot after the vault succeeds.
    pub fn confirm_erased(&self, slot: VaultSlot) -> Result<()> {
        self.0.conn().execute(
            "DELETE FROM credential_slots WHERE id = ?1 AND connection_id IS NULL",
            [slot.id.to_string()],
        )?;
        Ok(())
    }

    /// At startup, removes references to secrets held by a previous process.
    pub fn expire_sessions(&self) -> Result<()> {
        self.0.write_tx(|tx| {
            tx.execute(
                "UPDATE connections SET credential_state = ?1, last_check = NULL
                 WHERE id IN (SELECT connection_id FROM credential_slots WHERE session_only)",
                [CredentialState::Absent.as_str()],
            )?;
            tx.execute("DELETE FROM credential_slots WHERE session_only", [])?;
            Ok(())
        })
    }
}

fn slot(row: &Row<'_>) -> rusqlite::Result<VaultSlot> {
    let id: String = row.get("id")?;
    Ok(VaultSlot {
        id: id
            .parse()
            .map_err(|_| corrupt(0, "invalid credential slot id"))?,
        session_only: row.get("session_only")?,
    })
}
