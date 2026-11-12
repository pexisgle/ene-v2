//! Client rows and device-operation receipts.
use super::{Store, parse_json};
use crate::host::{
    client::{Client, DeviceOperation, DeviceResult, DeviceStatus},
    db::Result,
    error::HostError,
    failure::Failure,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

impl Store {
    pub(crate) fn client(&self, id: &str) -> Result<Option<Client>> {
        let id = id.to_owned();
        self.call(move |connection| read_client(connection, &id).map_err(Into::into))
    }

    pub(crate) fn clients(&self) -> Result<Vec<Client>> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, name, platform, capabilities, granted_capabilities, revision, last_seen \
                 FROM clients ORDER BY id",
            )?;
            statement
                .query_map([], row_to_client)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn save_client(&self, client: &Client) -> Result<()> {
        let client = client.clone();
        self.call(move |connection| {
            write_client(connection, &client)?;
            Ok(())
        })
    }

    pub(crate) fn device(&self, id: &str) -> Result<Option<DeviceOperation>> {
        let id = id.to_owned();
        self.call(move |connection| read_device(connection, &id).map_err(Into::into))
    }

    pub(crate) fn pending_devices(&self, client: &str) -> Result<Vec<DeviceOperation>> {
        let client = client.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {DEVICE_COLUMNS} FROM device_operations \
                 WHERE client_id=?1 AND status='pending' ORDER BY created_at"
            ))?;
            statement
                .query_map([&client], row_to_device)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn devices(&self) -> Result<Vec<DeviceOperation>> {
        self.call(|connection| {
            let mut statement = connection.prepare(&format!(
                "SELECT {DEVICE_COLUMNS} FROM device_operations ORDER BY created_at"
            ))?;
            statement
                .query_map([], row_to_device)?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn save_device(&self, operation: &DeviceOperation) -> Result<()> {
        let operation = operation.clone();
        self.call(move |connection| {
            write_device(connection, &operation)?;
            Ok(())
        })
    }
}

const DEVICE_COLUMNS: &str = "id, client_id, companion_id, capability, input, status, result, \
    subscription_id, created_at, updated_at";

fn read_client(connection: &Connection, id: &str) -> rusqlite::Result<Option<Client>> {
    connection
        .query_row(
            "SELECT id, name, platform, capabilities, granted_capabilities, revision, last_seen \
             FROM clients WHERE id=?1",
            [id],
            row_to_client,
        )
        .optional()
}

fn write_client(connection: &Connection, client: &Client) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO clients(id, name, platform, capabilities, granted_capabilities, revision, \
         last_seen) VALUES (?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(id) DO UPDATE SET name=excluded.name, platform=excluded.platform, \
         capabilities=excluded.capabilities, granted_capabilities=excluded.granted_capabilities, \
         revision=excluded.revision, last_seen=excluded.last_seen",
        params![
            client.id,
            client.name,
            client.platform,
            serde_json::to_string(&client.capabilities).map_err(json_error)?,
            serde_json::to_string(&client.granted_capabilities).map_err(json_error)?,
            client.revision,
            client.last_seen,
        ],
    )?;
    Ok(())
}

fn row_to_client(row: &rusqlite::Row<'_>) -> rusqlite::Result<Client> {
    let capabilities: String = row.get(3)?;
    let granted: String = row.get(4)?;
    Ok(Client {
        id: row.get(0)?,
        name: row.get(1)?,
        platform: row.get(2)?,
        capabilities: serde_json::from_str(&capabilities).map_err(json_error)?,
        granted_capabilities: serde_json::from_str(&granted).map_err(json_error)?,
        revision: row.get(5)?,
        last_seen: row.get(6)?,
    })
}

fn read_device(connection: &Connection, id: &str) -> rusqlite::Result<Option<DeviceOperation>> {
    connection
        .query_row(
            &format!("SELECT {DEVICE_COLUMNS} FROM device_operations WHERE id=?1"),
            [id],
            row_to_device,
        )
        .optional()
}

fn write_device(connection: &Connection, operation: &DeviceOperation) -> rusqlite::Result<()> {
    let result = operation.result.as_ref().map(device_result_json);
    connection.execute(
        "INSERT INTO device_operations(id, client_id, companion_id, capability, input, status, \
         result, subscription_id, created_at, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
         ON CONFLICT(id) DO UPDATE SET client_id=excluded.client_id, \
         companion_id=excluded.companion_id, capability=excluded.capability, input=excluded.input, \
         status=excluded.status, result=excluded.result, subscription_id=excluded.subscription_id, \
         updated_at=excluded.updated_at",
        params![
            operation.id,
            operation.client_id,
            operation.companion_id,
            operation.capability,
            operation.input.to_string(),
            operation.status.as_str(),
            result,
            operation.subscription_id,
            operation.created_at,
            operation.updated_at,
        ],
    )?;
    Ok(())
}

fn row_to_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceOperation> {
    let input: String = row.get(4)?;
    let status: String = row.get(5)?;
    let result: Option<String> = row.get(6)?;
    Ok(DeviceOperation {
        id: row.get(0)?,
        client_id: row.get(1)?,
        companion_id: row.get(2)?,
        capability: row.get(3)?,
        input: parse_json(&input).map_err(json_error)?,
        status: DeviceStatus::parse(&status),
        result: result
            .map(|value| parse_device_result(&value))
            .transpose()
            .map_err(json_error)?,
        subscription_id: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn device_result_json(result: &DeviceResult) -> String {
    json!({
        "success": result.success,
        "output": result.output,
        "failure": result.failure.as_ref().map(|failure| json!({
            "code": failure.code,
            "message": failure.message,
        })),
    })
    .to_string()
}

fn parse_device_result(value: &str) -> Result<DeviceResult> {
    let value: Value = parse_json(value)?;
    Ok(DeviceResult {
        success: value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        output: value.get("output").cloned().unwrap_or(Value::Null),
        failure: value.get("failure").and_then(|failure| {
            Some(Failure::new(
                failure.get("code")?.as_str()?,
                failure.get("message")?.as_str()?,
            ))
        }),
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
    pub(crate) fn activation(&self, companion_id: &str) -> Result<Option<(String, u64)>> {
        let companion_id = companion_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT client_id, revision FROM activations WHERE companion_id=?1",
                    [&companion_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub(crate) fn activations(&self) -> Result<Vec<(String, String, u64)>> {
        self.call(|connection| {
            let mut statement =
                connection.prepare("SELECT companion_id, client_id, revision FROM activations")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }

    pub(crate) fn save_activation(
        &self,
        companion_id: &str,
        client_id: &str,
        revision: u64,
    ) -> Result<()> {
        let companion_id = companion_id.to_owned();
        let client_id = client_id.to_owned();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO activations(companion_id, client_id, revision) VALUES (?1,?2,?3) \
                 ON CONFLICT(companion_id) DO UPDATE SET client_id=excluded.client_id, \
                 revision=excluded.revision",
                params![companion_id, client_id, revision],
            )?;
            Ok(())
        })
    }

    pub(crate) fn remove_activation(&self, companion_id: &str) -> Result<()> {
        let companion_id = companion_id.to_owned();
        self.call(move |connection| {
            connection.execute(
                "DELETE FROM activations WHERE companion_id=?1",
                [&companion_id],
            )?;
            Ok(())
        })
    }
}
