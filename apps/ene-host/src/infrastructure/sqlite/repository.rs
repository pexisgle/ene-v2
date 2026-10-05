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
    fn save_configuration(
        &self,
        c: ene_core::domain::Configuration,
        receipt: Option<ene_core::ports::ReceiptWrite>,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<()>> {
        Box::pin(async move {
            self.call(move |db| {
                let tx = db.transaction()?;
                conversation::write_configuration(&tx, &c)?;
                match receipt {
                    Some(ene_core::ports::ReceiptWrite::Create(r)) => {
                        conversation::save_receipt(&tx, &r.key, &r.target, &r.request,
                            stored_outcome(r.outcome), &r.response)?;
                    }
                    Some(ene_core::ports::ReceiptWrite::Complete(r)) => {
                        if !conversation::receipt(&tx, &r.key, &r.target, &r.request)?
                            .is_some_and(|(status, _)| status == 0) {
                            return Err(Error::StateConflict);
                        }
                        let changed = tx.execute(
                            "UPDATE receipts SET status=?1,response=?2 WHERE principal=(SELECT value FROM metadata WHERE key='principal') AND key=?3 AND target=?4 AND status=0",
                            params![stored_outcome(r.outcome),r.response.to_string(),r.key,r.target],
                        )?;
                        if changed != 1 { return Err(Error::StateConflict); }
                    }
                    None => {}
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(core_error)
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
        status: MessageStatus,
        delta: Option<String>,
        failure: Option<Failure>,
    ) -> futures_util::future::BoxFuture<'_, ene_core::ports::Result<Message>> {
        Box::pin(async move {
            self.call(move |db| conversation::update_message(db, &id, status, delta, failure))
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
