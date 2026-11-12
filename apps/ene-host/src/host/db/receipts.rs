//! Idempotent acceptance receipts.
use super::Store;
use crate::host::{
    db::Result,
    error::HostError,
    receipt::{Receipt, ReceiptKind, ReceiptStatus},
};
use rusqlite::{Connection, OptionalExtension, params};

impl Store {
    pub(crate) fn receipt(
        &self,
        kind: ReceiptKind,
        scope: &str,
        request_key: &str,
    ) -> Result<Option<Receipt>> {
        let scope = scope.to_owned();
        let request_key = request_key.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT hash, status, request_id, updated_at FROM receipts \
                     WHERE kind=?1 AND scope_id=?2 AND request_key=?3",
                    params![kind.as_str(), scope, request_key],
                    row_to_receipt,
                )
                .optional()
                .map_err(Into::into)
        })
    }

    pub(crate) fn save_receipt(
        &self,
        kind: ReceiptKind,
        scope: &str,
        request_key: &str,
        receipt: &Receipt,
    ) -> Result<()> {
        let scope = scope.to_owned();
        let request_key = request_key.to_owned();
        let receipt = receipt.clone();
        self.call(move |connection| {
            write_receipt(connection, kind.as_str(), &scope, &request_key, &receipt)?;
            Ok(())
        })
    }

    pub(crate) fn pending_receipts(
        &self,
        kind: ReceiptKind,
    ) -> Result<Vec<(String, String, Receipt)>> {
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT scope_id, request_key, hash, status, request_id, updated_at \
                 FROM receipts WHERE kind=?1 AND status=?2 ORDER BY updated_at",
            )?;
            statement
                .query_map(
                    params![kind.as_str(), ReceiptStatus::Pending.as_str()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            Receipt {
                                hash: row.get(2)?,
                                status: ReceiptStatus::parse(row.get::<_, String>(3)?.as_str())
                                    .unwrap_or(ReceiptStatus::Pending),
                                request_id: row.get(4)?,
                                updated_at: row.get(5)?,
                            },
                        ))
                    },
                )?
                .map(|row| row.map_err(Into::into))
                .collect()
        })
    }
}

pub(super) fn write_receipt(
    connection: &Connection,
    kind: &str,
    scope: &str,
    request_key: &str,
    receipt: &Receipt,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO receipts(kind, scope_id, request_key, hash, status, request_id, updated_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7) \
         ON CONFLICT(kind, scope_id, request_key) DO UPDATE SET hash=excluded.hash, \
         status=excluded.status, request_id=excluded.request_id, updated_at=excluded.updated_at",
        params![
            kind,
            scope,
            request_key,
            receipt.hash,
            receipt.status.as_str(),
            receipt.request_id,
            receipt.updated_at,
        ],
    )?;
    Ok(())
}

fn row_to_receipt(row: &rusqlite::Row<'_>) -> rusqlite::Result<Receipt> {
    let status: String = row.get(1)?;
    Ok(Receipt {
        hash: row.get(0)?,
        status: ReceiptStatus::parse(&status)
            .ok_or_else(|| HostError::storage("invalid receipt status").into_sql())?,
        request_id: row.get(2)?,
        updated_at: row.get(3)?,
    })
}

trait IntoSql {
    fn into_sql(self) -> rusqlite::Error;
}

impl IntoSql for HostError {
    fn into_sql(self) -> rusqlite::Error {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(self))
    }
}
