//! Device authorization records. Each device and operation is one row, so a
//! change rewrites that row instead of the whole journal.
use super::{OptionalExtension, Result, Store, params};
use ene_rpc::rpc as w;

const REVISION: &str = "device_activation_revision";

fn code(status: w::OperationStatus) -> i32 {
    status as i32
}

fn encode(record: &impl serde::Serialize) -> std::result::Result<String, super::Error> {
    serde_json::to_string(record).map_err(Into::into)
}

fn decode<T: serde::de::DeserializeOwned>(body: &str) -> std::result::Result<T, super::Error> {
    serde_json::from_str(body).map_err(Into::into)
}

impl Store {
    /// The device `id`, as the host saved it.
    pub async fn device_get(&self, id: String) -> Result<Option<w::Device>> {
        self.call(move |db| {
            let body: Option<String> = db
                .query_row("SELECT body FROM devices WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            body.as_deref().map(decode).transpose()
        })
        .await
    }

    /// Every device with the token hash that authenticates it.
    pub async fn device_records(&self) -> Result<Vec<(w::Device, Vec<u8>)>> {
        self.call(|db| {
            let mut query = db.prepare("SELECT body, token_hash FROM devices ORDER BY id")?;
            let rows = query
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            rows.iter()
                .map(|(body, hash)| Ok((decode::<w::Device>(body)?, hash.clone())))
                .collect()
        })
        .await
    }

    pub async fn device_register(&self, device: w::Device, token_hash: Vec<u8>) -> Result<()> {
        let body = encode(&device)?;
        self.call(move |db| {
            db.execute(
                "INSERT INTO devices(id,token_hash,body) VALUES (?1,?2,?3)",
                params![device.id, token_hash, body],
            )?;
            Ok(())
        })
        .await
    }

    /// Writes a changed device. `revoke` also marks the device's dispatched
    /// operations revoked, in the same transaction.
    pub async fn device_write(
        &self,
        device: w::Device,
        capability: Option<String>,
        revoke: bool,
    ) -> Result<()> {
        let body = encode(&device)?;
        self.call(move |db| {
            let tx = db.transaction()?;
            tx.execute(
                "UPDATE devices SET body=?1 WHERE id=?2",
                params![body, device.id],
            )?;
            if revoke {
                let mut query = tx.prepare(
                    "SELECT operation_id, body FROM device_operations WHERE device_id=?1 AND status=?2",
                )?;
                let rows = query
                    .query_map(params![device.id, code(w::OperationStatus::Dispatched)], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                drop(query);
                for (operation_id, body) in rows {
                    let mut record = decode::<w::OperationRecord>(&body)?;
                    if capability
                        .as_ref()
                        .is_some_and(|wanted| record.request.capability != *wanted)
                    {
                        continue;
                    }
                    let Some(state) = record.state.as_option_mut() else {
                        continue;
                    };
                    state.status = w::OperationStatus::Revoked.into();
                    tx.execute(
                        "UPDATE device_operations SET status=?1, body=?2 WHERE operation_id=?3",
                        params![
                            code(w::OperationStatus::Revoked),
                            encode(&record)?,
                            operation_id
                        ],
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn device_operations(&self) -> Result<i64> {
        self.call(
            |db| Ok(db.query_row("SELECT COUNT(*) FROM device_operations", [], |r| r.get(0))?),
        )
        .await
    }

    pub async fn device_operation(
        &self,
        operation_id: String,
    ) -> Result<Option<w::OperationRecord>> {
        self.call(move |db| {
            let body: Option<String> = db
                .query_row(
                    "SELECT body FROM device_operations WHERE operation_id=?1",
                    [operation_id],
                    |r| r.get(0),
                )
                .optional()?;
            body.as_deref().map(decode).transpose()
        })
        .await
    }

    /// Records a dispatched operation. `false` when its id was already used.
    pub async fn device_operation_insert(&self, record: w::OperationRecord) -> Result<bool> {
        let body = encode(&record)?;
        let operation_id = record.request.operation_id.clone();
        let device_id = record.request.device_id.clone();
        let subscription_id = record.subscription_id.clone();
        self.call(move |db| {
            let inserted = db.execute(
                "INSERT INTO device_operations(operation_id,device_id,subscription_id,status,body) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(operation_id) DO NOTHING",
                params![
                    operation_id,
                    device_id,
                    subscription_id,
                    code(w::OperationStatus::Dispatched),
                    body
                ],
            )?;
            Ok(inserted == 1)
        })
        .await
    }

    /// Settles a dispatched operation of `subscription_id`. `false` when the
    /// row is no longer dispatched by that subscription.
    pub async fn device_operation_complete(&self, record: w::OperationRecord) -> Result<bool> {
        let body = encode(&record)?;
        let operation_id = record.request.operation_id.clone();
        let completed = code(w::OperationStatus::Completed);
        self.call(move |db| {
            let updated = db.execute(
                "UPDATE device_operations SET status=?1, body=?2 WHERE operation_id=?3 AND status=?4 AND subscription_id=?5",
                params![
                    completed,
                    body,
                    operation_id,
                    code(w::OperationStatus::Dispatched),
                    record.subscription_id
                ],
            )?;
            Ok(updated == 1)
        })
        .await
    }

    /// Marks operations that were dispatched when the host stopped as
    /// uncertain, and returns the activation revision they were recorded with.
    pub async fn device_recover(&self) -> Result<u64> {
        self.call(|db| {
            let tx = db.transaction()?;
            let mut query =
                tx.prepare("SELECT operation_id, body FROM device_operations WHERE status=?1")?;
            let rows = query
                .query_map([code(w::OperationStatus::Dispatched)], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(query);
            for (operation_id, body) in rows {
                let mut record = decode::<w::OperationRecord>(&body)?;
                if let Some(state) = record.state.as_option_mut() {
                    state.status = w::OperationStatus::Uncertain.into();
                }
                tx.execute(
                    "UPDATE device_operations SET status=?1, body=?2 WHERE operation_id=?3",
                    params![
                        code(w::OperationStatus::Uncertain),
                        encode(&record)?,
                        operation_id
                    ],
                )?;
            }
            let revision: String = super::queries::meta(&tx, REVISION).unwrap_or_default();
            tx.commit()?;
            Ok(revision.parse().unwrap_or_default())
        })
        .await
    }

    /// Records a new activation revision; the leases themselves are memory.
    pub async fn device_activate(&self, revision: u64) -> Result<()> {
        self.call(move |db| {
            db.execute(
                "INSERT INTO metadata VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![REVISION, revision.to_string()],
            )?;
            Ok(())
        })
        .await
    }
}
