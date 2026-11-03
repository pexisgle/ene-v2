use super::*;

impl Store {
    pub async fn snapshot(&self) -> crate::ports::Result<StoredSnapshot> {
        self.call(snapshot).await.map_err(core_error)
    }
    pub async fn configuration(&self) -> crate::ports::Result<crate::domain::Configuration> {
        self.call(|db| conversation::load(db))
            .await
            .map_err(core_error)
    }
    pub async fn commit_configuration(
        &self,
        change: crate::ports::ConfigurationCommit,
    ) -> crate::ports::Result<()> {
        self.call(move |db| {
            use crate::ports::ConfigurationCommit;
            let tx = db.transaction()?;
            match change {
                ConfigurationCommit::Update(c) => conversation::write_configuration(&tx, &c)?,
                ConfigurationCommit::CreateGrant {
                    configuration,
                    receipt,
                } => {
                    conversation::write_configuration(&tx, &configuration)?;
                    conversation::save_receipt(
                        &tx,
                        &receipt.key,
                        &receipt.target,
                        &receipt.request,
                        stored_outcome(receipt.outcome),
                        &receipt.response,
                    )?;
                }
                ConfigurationCommit::CompleteCheck {
                    configuration,
                    receipt,
                } => {
                    if !matches!(
                        receipt.outcome,
                        crate::ports::Outcome::Completed | crate::ports::Outcome::Unavailable
                    ) {
                        return Err(Error::StateConflict);
                    }
                    let request_id =
                        crate::infrastructure::sqlite::conversation::request_id(&receipt.response)
                            .filter(|id| !id.is_empty())
                            .ok_or(Error::StateConflict)?;
                    // Check identity and Pending state before changing either record.
                    let previous = conversation::receipt(
                        &tx,
                        &receipt.key,
                        &receipt.target,
                        &receipt.request,
                    )?
                    .ok_or(Error::StateConflict)?;
                    if previous.0 != 0 {
                        if previous == (stored_outcome(receipt.outcome), receipt.response.clone()) {
                            return Ok(());
                        }
                        return Err(Error::StateConflict);
                    }
                    if crate::infrastructure::sqlite::conversation::request_id(&previous.1)
                        != Some(request_id)
                    {
                        return Err(Error::StateConflict);
                    }
                    conversation::write_configuration(&tx, &configuration)?;
                    conversation::complete_receipt(
                        &tx,
                        &receipt.key,
                        &receipt.target,
                        &receipt.request,
                        stored_outcome(receipt.outcome),
                        &receipt.response,
                    )?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(core_error)
    }
    pub async fn receipt(
        &self,
        key: String,
        target: String,
        request: serde_json::Value,
    ) -> crate::ports::Result<Option<(crate::ports::Outcome, serde_json::Value)>> {
        let r = self
            .call(move |db| conversation::receipt(db, &key, &target, &request))
            .await
            .map_err(core_error)?;
        r.map(|(n, v)| Ok((receipt_outcome(n)?, v))).transpose()
    }
    pub async fn reserve(&self, r: crate::ports::Receipt) -> crate::ports::Result<()> {
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
    }
    pub async fn complete_receipt(&self, r: crate::ports::Receipt) -> crate::ports::Result<()> {
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
    }
    pub async fn release_receipt(
        &self,
        key: String,
        target: String,
        request: serde_json::Value,
    ) -> crate::ports::Result<()> {
        self.call(move |db| conversation::release_receipt(db, &key, &target, &request))
            .await
            .map_err(core_error)
    }
    pub async fn mind_view(
        &self,
        companion: String,
        now: String,
    ) -> crate::ports::Result<crate::domain::mind::StoredMind> {
        self.call(move |db| mind::view(db, &companion, &now))
            .await
            .map_err(core_error)
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn mind_update(
        &self,
        owner: String,
        operation_id: String,
        request_hash: String,
        request: crate::domain::mind::MindUpdateRequest,
        sources: Vec<crate::domain::mind::SourceRef>,
        actor: String,
        now: String,
    ) -> crate::ports::Result<crate::domain::mind::MindUpdateResult> {
        self.call(move |db| {
            mind::update(
                db,
                &owner,
                &operation_id,
                &request_hash,
                &request,
                &sources,
                &actor,
                &now,
            )
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_recall(
        &self,
        owner: String,
        query: String,
        limit: usize,
    ) -> crate::ports::Result<Vec<crate::domain::mind::MemoryRecord>> {
        self.call(move |db| mind::recall(db, &owner, &query, limit))
            .await
            .map_err(core_error)
    }
    pub async fn mind_read(
        &self,
        owner: String,
        id: String,
    ) -> crate::ports::Result<Option<crate::domain::mind::MemoryRecord>> {
        self.call(move |db| mind::read(db, &owner, &id))
            .await
            .map_err(core_error)
    }
    pub async fn mind_update_profile(
        &self,
        companion: String,
        expected_revision: u64,
        profile: crate::domain::mind::ProfileDocument,
        now: String,
    ) -> crate::ports::Result<(u64, crate::domain::mind::ProfileDocument)> {
        self.call(move |db| mind::update_profile(db, &companion, expected_revision, &profile, &now))
            .await
            .map_err(core_error)
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
