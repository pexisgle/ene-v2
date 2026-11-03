use super::*;

impl crate::ports::Repository for Store {
    fn snapshot(
        &self,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<StoredSnapshot>> {
        Box::pin(async move { self.snapshot().await.map_err(core_error) })
    }
    fn configuration(
        &self,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<crate::domain::Configuration>>
    {
        Box::pin(async move {
            self.call(|db| conversation::load(db))
                .await
                .map_err(core_error)
        })
    }
    fn commit_configuration(
        &self,
        change: crate::ports::ConfigurationCommit,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| {
                use crate::ports::ConfigurationCommit;
                let tx = db.transaction()?;
                match change {
                    ConfigurationCommit::Update(c) => conversation::write_configuration(&tx, &c)?,
                    ConfigurationCommit::CreateGrant { configuration, receipt } => {
                        conversation::write_configuration(&tx, &configuration)?;
                        conversation::save_receipt(&tx, &receipt.key, &receipt.target, &receipt.request,
                            stored_outcome(receipt.outcome), &receipt.response)?;
                    }
                    ConfigurationCommit::CompleteCheck { configuration, receipt } => {
                        if !matches!(receipt.outcome, crate::ports::Outcome::Completed | crate::ports::Outcome::Unavailable) { return Err(Error::StateConflict); }
                        let request_id = receipt.response.get("requestId").or_else(|| receipt.response.get("request_id")).and_then(serde_json::Value::as_str).filter(|id| !id.is_empty()).ok_or(Error::StateConflict)?;
                        // Check identity and Pending state before changing either record.
                        let previous = conversation::receipt(&tx, &receipt.key, &receipt.target, &receipt.request)?
                            .ok_or(Error::StateConflict)?;
                        if previous.0 != 0 {
                            if previous == (stored_outcome(receipt.outcome), receipt.response.clone()) { return Ok(()); }
                            return Err(Error::StateConflict);
                        }
                        if previous.1.get("requestId").or_else(|| previous.1.get("request_id")).and_then(serde_json::Value::as_str) != Some(request_id) { return Err(Error::StateConflict); }
                        conversation::write_configuration(&tx, &configuration)?;
                        let rows = tx.execute(
                            "UPDATE receipts SET status=?1,response=?2 WHERE principal=(SELECT value FROM metadata WHERE key='principal') AND key=?3 AND target=?4 AND method='POST' AND status=0",
                            params![stored_outcome(receipt.outcome), receipt.response.to_string(), receipt.key, receipt.target],
                        )?;
                        if rows != 1 { return Err(Error::StateConflict); }
                    }
                }
                tx.commit()?;
                Ok(())
            }).await.map_err(core_error)
        })
    }
    fn receipt(
        &self,
        key: String,
        target: String,
        request: serde_json::Value,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<Option<(crate::ports::Outcome, serde_json::Value)>>,
    > {
        Box::pin(async move {
            let r = self
                .call(move |db| conversation::receipt(db, &key, &target, &request))
                .await
                .map_err(core_error)?;
            r.map(|(n, v)| Ok((receipt_outcome(n)?, v))).transpose()
        })
    }
    fn reserve(
        &self,
        r: crate::ports::Receipt,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| {
                conversation::save_receipt(
                    db,
                    &r.key,
                    &r.target,
                    &r.request,
                    stored_outcome(r.outcome),
                    &r.response,
                )
            })
            .await
            .map_err(core_error)
        })
    }
    fn complete_receipt(
        &self,
        r: crate::ports::Receipt,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| {
                conversation::complete_receipt(
                    db,
                    &r.key,
                    &r.target,
                    &r.request,
                    stored_outcome(r.outcome),
                    &r.response,
                )
            })
            .await
            .map_err(core_error)
        })
    }
    fn release_receipt(
        &self,
        key: String,
        target: String,
        request: serde_json::Value,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| conversation::release_receipt(db, &key, &target, &request))
                .await
                .map_err(core_error)
        })
    }

    fn mind_view(
        &self,
        companion: String,
        now: String,
    ) -> futures_util::future::BoxFuture<'_, crate::ports::Result<crate::domain::mind::StoredMind>>
    {
        Box::pin(async move {
            self.call(move |db| mind::view(db, &companion, &now))
                .await
                .map_err(core_error)
        })
    }
    fn mind_update(
        &self,
        owner: String,
        operation_id: String,
        request_hash: String,
        request: crate::domain::mind::MindUpdateRequest,
        source: crate::domain::mind::SourceRef,
        now: String,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<crate::domain::mind::MindUpdateResult>,
    > {
        Box::pin(async move {
            self.call(move |db| {
                mind::update(
                    db,
                    &owner,
                    &operation_id,
                    &request_hash,
                    &request,
                    &source,
                    &now,
                )
            })
            .await
            .map_err(core_error)
        })
    }
    fn mind_recall(
        &self,
        owner: String,
        query: String,
        limit: usize,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<Vec<crate::domain::mind::MemoryRecord>>,
    > {
        Box::pin(async move {
            self.call(move |db| mind::recall(db, &owner, &query, limit))
                .await
                .map_err(core_error)
        })
    }
    fn mind_read(
        &self,
        owner: String,
        id: String,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<Option<crate::domain::mind::MemoryRecord>>,
    > {
        Box::pin(async move {
            self.call(move |db| mind::read(db, &owner, &id))
                .await
                .map_err(core_error)
        })
    }
    fn mind_update_profile(
        &self,
        companion: String,
        expected_revision: u64,
        profile: crate::domain::mind::ProfileDocument,
        now: String,
    ) -> futures_util::future::BoxFuture<
        '_,
        crate::ports::Result<(u64, crate::domain::mind::ProfileDocument)>,
    > {
        Box::pin(async move {
            self.call(move |db| {
                mind::update_profile(db, &companion, expected_revision, &profile, &now)
            })
            .await
            .map_err(core_error)
        })
    }
}
pub(crate) fn core_error(error: Error) -> crate::domain::Error {
    use crate::domain::{Error as CoreError, ErrorKind};
    match error {
        Error::IdempotencyConflict => CoreError::new(ErrorKind::IdempotencyConflict),
        Error::StateConflict => CoreError::new(ErrorKind::StateConflict),
        Error::RevisionConflict => CoreError::new(ErrorKind::RevisionConflict),
        Error::UnknownReference => CoreError {
            kind: ErrorKind::InvalidRequest,
            reason: Some("unknown_reference"),
        },
        _ => CoreError::new(ErrorKind::StorageFailure),
    }
}

fn stored_outcome(outcome: crate::ports::Outcome) -> u16 {
    use crate::ports::Outcome;
    match outcome {
        Outcome::Pending => 0,
        Outcome::Completed => 200,
        Outcome::Created => 201,
        Outcome::Accepted => 202,
        Outcome::Unavailable => 503,
    }
}
fn receipt_outcome(n: u16) -> crate::ports::Result<crate::ports::Outcome> {
    use crate::ports::Outcome;
    Ok(match n {
        0 => Outcome::Pending,
        200 => Outcome::Completed,
        201 => Outcome::Created,
        202 => Outcome::Accepted,
        503 => Outcome::Unavailable,
        _ => {
            return Err(crate::domain::Error::new(
                crate::domain::ErrorKind::StorageFailure,
            ));
        }
    })
}
