//! Settings, connection, policy and approval rows.
use super::{Store, parse_json};
use crate::host::{
    db::Result,
    error::HostError,
    settings::{Connection, CredentialState, Settings},
};
use ene_rpc as w;
use rusqlite::{Connection as SqlConnection, OptionalExtension, params};

impl Store {
    pub(crate) fn settings(&self) -> Result<Settings> {
        self.call(|connection| {
            let (revision, updated_at): (u64, String) = connection.query_row(
                "SELECT revision, updated_at FROM settings WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok(Settings {
                revision,
                connections: read_connections(connection)?,
                updated_at,
            })
        })
    }

    pub(crate) fn settings_revision(&self) -> Result<u64> {
        self.call(|connection| {
            connection
                .query_row("SELECT revision FROM settings WHERE id=1", [], |row| {
                    row.get(0)
                })
                .map_err(Into::into)
        })
    }

    pub(crate) fn connection(&self, id: &str) -> Result<Option<Connection>> {
        let id = id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    &format!("SELECT {CONNECTION_COLUMNS} FROM connections WHERE id=?1"),
                    [&id],
                    row_to_connection,
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub(crate) fn save_connection(&self, connection: &Connection) -> Result<()> {
        let connection = connection.clone();
        self.call(move |sql| {
            write_connection(sql, &connection)?;
            Ok(())
        })
    }

    pub(crate) fn replace_connections(
        &self,
        revision: u64,
        updated_at: &str,
        connections: &[Connection],
        _removed: &[String],
    ) -> Result<()> {
        let connections = connections.to_vec();
        let updated_at = updated_at.to_owned();
        self.call(move |sql| {
            let transaction = sql.transaction()?;
            transaction.execute("DELETE FROM connections", [])?;
            for connection in &connections {
                write_connection(&transaction, connection)?;
            }
            transaction.execute(
                "UPDATE settings SET revision=?1, updated_at=?2 WHERE id=1",
                params![revision, updated_at],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub(crate) fn policy_rules(&self) -> Result<Vec<w::PolicyRule>> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, tool_name, pattern, effect, created_at FROM policy_rules \
                 ORDER BY created_at, id",
            )?;
            statement
                .query_map([], row_to_policy)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn update_policy_rules(
        &self,
        add_or_update: &[w::PolicyRule],
        remove_ids: &[String],
    ) -> Result<()> {
        let add_or_update = add_or_update.to_vec();
        let remove_ids = remove_ids.to_vec();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            for id in &remove_ids {
                transaction.execute("DELETE FROM policy_rules WHERE id=?1", [id])?;
            }
            for rule in &add_or_update {
                if !matches!(
                    rule.effect.as_str(),
                    "allow" | "deny" | "confirm" | "always_approve"
                ) {
                    return Err(HostError::invalid("invalid policy effect"));
                }
                let created_at = if rule.created_at.is_empty() {
                    crate::host::db::now()
                } else {
                    rule.created_at.clone()
                };
                transaction.execute(
                    "INSERT INTO policy_rules(id, tool_name, pattern, effect, created_at) \
                     VALUES (?1,?2,?3,?4,?5) \
                     ON CONFLICT(id) DO UPDATE SET tool_name=excluded.tool_name, \
                     pattern=excluded.pattern, effect=excluded.effect",
                    params![
                        rule.id,
                        rule.tool_name,
                        rule.pattern,
                        rule.effect,
                        created_at
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub(crate) fn pending_approvals(&self) -> Result<Vec<w::ApprovalRequest>> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT request_id, session_id, run_id, action_payload_hash, tool_name, \
                 normalized_arguments, status, created_at FROM approval_requests \
                 WHERE status='pending' ORDER BY created_at",
            )?;
            statement
                .query_map([], row_to_approval)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn set_approval_status(&self, request_id: &str, status: &str) -> Result<()> {
        let request_id = request_id.to_owned();
        let status = status.to_owned();
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE approval_requests SET status=?2 WHERE request_id=?1",
                params![request_id, status],
            )?;
            if changed == 0 {
                return Err(HostError::not_found("approval request not found"));
            }
            Ok(())
        })
    }
}

const CONNECTION_COLUMNS: &str =
    "id, kind, adapter, endpoint, options, credential_ref, credential_state, last_check, enabled";

fn read_connections(connection: &SqlConnection) -> rusqlite::Result<Vec<Connection>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {CONNECTION_COLUMNS} FROM connections ORDER BY id"
    ))?;
    statement.query_map([], row_to_connection)?.collect()
}

fn write_connection(connection: &SqlConnection, value: &Connection) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO connections(id, kind, adapter, endpoint, options, credential_ref, \
         credential_state, last_check, enabled) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
         ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, adapter=excluded.adapter, \
         endpoint=excluded.endpoint, options=excluded.options, \
         credential_ref=excluded.credential_ref, credential_state=excluded.credential_state, \
         last_check=excluded.last_check, enabled=excluded.enabled",
        params![
            value.id,
            value.kind,
            value.adapter,
            value.endpoint,
            value.options.to_string(),
            value.credential_ref,
            value.credential_state.as_str(),
            value.last_check.to_string(),
            i64::from(value.enabled),
        ],
    )?;
    Ok(())
}

fn row_to_connection(row: &rusqlite::Row<'_>) -> rusqlite::Result<Connection> {
    let options: String = row.get(4)?;
    let state: String = row.get(6)?;
    let last_check: String = row.get(7)?;
    Ok(Connection {
        id: row.get(0)?,
        kind: row.get(1)?,
        adapter: row.get(2)?,
        endpoint: row.get(3)?,
        options: parse_json(&options).map_err(json_error)?,
        credential_ref: row.get(5)?,
        credential_state: CredentialState::parse(&state),
        last_check: parse_json(&last_check).map_err(json_error)?,
        enabled: row.get::<_, i64>(8)? != 0,
    })
}

fn row_to_policy(row: &rusqlite::Row<'_>) -> rusqlite::Result<w::PolicyRule> {
    Ok(w::PolicyRule {
        id: row.get(0)?,
        tool_name: row.get(1)?,
        pattern: row.get(2)?,
        effect: row.get(3)?,
        created_at: row.get(4)?,
        ..Default::default()
    })
}

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<w::ApprovalRequest> {
    Ok(w::ApprovalRequest {
        request_id: row.get(0)?,
        session_id: row.get(1)?,
        run_id: row.get(2)?,
        action_payload_hash: row.get(3)?,
        tool_name: row.get(4)?,
        normalized_arguments: row.get(5)?,
        status: row.get(6)?,
        created_at: row.get(7)?,
        ..Default::default()
    })
}

fn json_error(error: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(HostError::storage(error.to_string())),
    )
}

impl Store {
    pub(crate) fn approval_for(
        &self,
        tool_name: &str,
        hash: &[u8],
        session_id: &str,
        run_id: &str,
    ) -> Result<Option<w::ApprovalRequest>> {
        let tool_name = tool_name.to_owned();
        let hash = hash.to_vec();
        let session_id = session_id.to_owned();
        let run_id = run_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT request_id, session_id, run_id, action_payload_hash, tool_name, \
                     normalized_arguments, status, created_at FROM approval_requests \
                     WHERE tool_name=?1 AND action_payload_hash=?2 AND session_id=?3 \
                       AND run_id=?4 \
                     ORDER BY created_at DESC LIMIT 1",
                    params![tool_name, hash, session_id, run_id],
                    row_to_approval,
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub(crate) fn consume_approval(&self, request_id: &str) -> Result<bool> {
        let request_id = request_id.to_owned();
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE approval_requests SET status='consumed' \
                 WHERE request_id=?1 AND status='approved'",
                [request_id],
            )?;
            Ok(changed > 0)
        })
    }

    pub(crate) fn save_approval(&self, request: &w::ApprovalRequest) -> Result<()> {
        let request = request.clone();
        self.call(move |connection| {
            connection.execute(
                "INSERT OR IGNORE INTO approval_requests(request_id, session_id, run_id, \
                 action_payload_hash, tool_name, normalized_arguments, status, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    request.request_id,
                    request.session_id,
                    request.run_id,
                    request.action_payload_hash,
                    request.tool_name,
                    request.normalized_arguments,
                    request.status,
                    request.created_at,
                ],
            )?;
            Ok(())
        })
    }
}

impl Store {
    pub(crate) fn approval(&self, request_id: &str) -> Result<Option<w::ApprovalRequest>> {
        let request_id = request_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT request_id, session_id, run_id, action_payload_hash, tool_name, \
                     normalized_arguments, status, created_at FROM approval_requests \
                     WHERE request_id=?1",
                    [&request_id],
                    row_to_approval,
                )
                .optional()
                .map_err(Into::into)
        })
    }
}

impl Store {
    pub(crate) fn bump_settings(&self) -> Result<u64> {
        self.call(|connection| {
            connection.execute(
                "UPDATE settings SET revision=revision+1, updated_at=?1 WHERE id=1",
                [crate::host::db::now()],
            )?;
            connection
                .query_row("SELECT revision FROM settings WHERE id=1", [], |row| {
                    row.get(0)
                })
                .map_err(Into::into)
        })
    }
}
