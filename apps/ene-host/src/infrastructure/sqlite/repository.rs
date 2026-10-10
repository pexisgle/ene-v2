use super::*;

impl ene_core::ports::Repository for Store {
    fn snapshot(
        &self,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<StoredSnapshot>> {
        Box::pin(async move { self.snapshot().await.map_err(core_error) })
    }
    fn messages(
        &self,
        c: String,
        cursor: Option<String>,
        limit: usize,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Page<Message>>> {
        Box::pin(async move { self.messages(c, cursor, limit).await.map_err(core_error) })
    }
    fn message(
        &self,
        c: String,
        id: String,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Option<Message>>> {
        Box::pin(async move { self.message(c, id).await.map_err(core_error) })
    }
    fn configuration(
        &self,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<ene_core::domain::Configuration>>
    {
        Box::pin(async move {
            self.call(|db| conversation::load(db))
                .await
                .map_err(core_error)
        })
    }
    fn commit_configuration(
        &self,
        change: ene_core::ports::ConfigurationCommit,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| {
                use ene_core::ports::ConfigurationCommit;
                let tx = db.transaction()?;
                match change {
                    ConfigurationCommit::Update(c) => conversation::write_configuration(&tx, &c)?,
                    ConfigurationCommit::CreateGrant { configuration, receipt } => {
                        conversation::write_configuration(&tx, &configuration)?;
                        conversation::save_receipt(&tx, &receipt.key, &receipt.target, &receipt.request,
                            stored_outcome(receipt.outcome), &receipt.response)?;
                    }
                    ConfigurationCommit::CompleteCheck { configuration, receipt } => {
                        if !matches!(receipt.outcome, ene_core::ports::Outcome::Completed | ene_core::ports::Outcome::Unavailable) { return Err(Error::StateConflict); }
                        let request_id = receipt.response["request_id"].as_str().filter(|id| !id.is_empty()).ok_or(Error::StateConflict)?;
                        // Check identity and Pending state before changing either record.
                        let previous = conversation::receipt(&tx, &receipt.key, &receipt.target, &receipt.request)?
                            .ok_or(Error::StateConflict)?;
                        if previous.0 != 0 {
                            if previous == (stored_outcome(receipt.outcome), receipt.response.clone()) { return Ok(()); }
                            return Err(Error::StateConflict);
                        }
                        if previous.1["request_id"].as_str() != Some(request_id) { return Err(Error::StateConflict); }
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
        ene_core::ports::Result<Option<(ene_core::ports::Outcome, serde_json::Value)>>,
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
        r: ene_core::ports::Receipt,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
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
    fn accept(
        &self,
        key: String,
        companion: String,
        request: serde_json::Value,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<ene_core::ports::Accepted>>
    {
        Box::pin(async move {
            self.call(move |db| conversation::accept(db, &key, &companion, request))
                .await
                .map_err(core_error)
        })
    }
    fn update_message(
        &self,
        id: String,
        update: ene_core::domain::MessageUpdate,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Message>> {
        Box::pin(async move {
            self.call(move |db| conversation::update_message(db, &id, update))
                .await
                .map_err(core_error)
        })
    }
}
fn core_error(error: Error) -> ene_core::domain::Error {
    use ene_core::domain::{Error as CoreError, ErrorKind};
    match error {
        Error::InvalidCursor => CoreError::invalid(),
        Error::IdempotencyConflict => CoreError::new(ErrorKind::IdempotencyConflict),
        Error::StateConflict => CoreError::new(ErrorKind::StateConflict),
        Error::ResourceLimit | Error::OversizedMessage => CoreError::new(ErrorKind::ResourceLimit),
        _ => CoreError::new(ErrorKind::StorageFailure),
    }
}

fn stored_outcome(outcome: ene_core::ports::Outcome) -> u16 {
    use ene_core::ports::Outcome;
    match outcome {
        Outcome::Pending => 0,
        Outcome::Completed => 200,
        Outcome::Created => 201,
        Outcome::Accepted => 202,
        Outcome::Unavailable => 503,
    }
}
fn receipt_outcome(n: u16) -> ene_core::ports::Result<ene_core::ports::Outcome> {
    use ene_core::ports::Outcome;
    Ok(match n {
        0 => Outcome::Pending,
        200 => Outcome::Completed,
        201 => Outcome::Created,
        202 => Outcome::Accepted,
        503 => Outcome::Unavailable,
        _ => {
            return Err(ene_core::domain::Error::new(
                ene_core::domain::ErrorKind::StorageFailure,
            ));
        }
    })
}
