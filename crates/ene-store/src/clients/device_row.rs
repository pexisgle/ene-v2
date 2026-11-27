//! Reads device operations and their persisted owner scope.
use crate::{
    corrupt, enum_name_column, id_column, json_column, optional_json_column, row::FromRow,
};
use ene_domain::{DeviceOperation, DeviceScope};
use rusqlite::Row;

pub(super) const OPERATION_COLUMNS: &str = "operation_id, client_id, capability, input, \
     companion_id, work_id, run_id, status, output, failure, created_at, updated_at, \
     dispatch_subscription, activation_generation";

impl FromRow for DeviceOperation {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let status = enum_name_column(row, "status", "unknown device status")?;
        let has_companion = row.get::<_, Option<String>>("companion_id")?.is_some();
        let work_id = row.get("work_id")?;
        let run_id = row.get("run_id")?;
        let scope = match (has_companion, work_id, run_id) {
            (false, work_id, None) => DeviceScope::Legacy { work_id },
            (true, None, None) => DeviceScope::Companion {
                companion_id: id_column(row, "companion_id", "invalid device owner")?,
                activation_generation: row.get("activation_generation")?,
            },
            (true, Some(work_id), Some(run_id)) => DeviceScope::Work {
                companion_id: id_column(row, "companion_id", "invalid device owner")?,
                work_id,
                run_id,
            },
            _ => {
                return Err(corrupt(
                    row.as_ref().column_index("run_id")?,
                    "invalid device scope",
                ));
            }
        };
        Ok(DeviceOperation {
            id: row.get("operation_id")?,
            client_id: row.get("client_id")?,
            capability: row.get("capability")?,
            input: json_column(row, "input", "stored operation JSON is corrupt")?,
            scope,
            status,
            output: optional_json_column(row, "output", "stored operation JSON is corrupt")?,
            failure: optional_json_column(row, "failure", "stored operation JSON is corrupt")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}
