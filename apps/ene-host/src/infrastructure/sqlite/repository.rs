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
    #[allow(clippy::too_many_arguments)]
    pub async fn mind_search(
        &self,
        caller: String,
        query: String,
        vector: Option<Vec<f32>>,
        model: Option<String>,
        limit: usize,
        include_forgotten: bool,
        since: Option<String>,
        until: Option<String>,
        now: String,
    ) -> crate::ports::Result<memory::SearchResult> {
        self.call(move |db| {
            memory::search(
                db,
                &caller,
                &query,
                vector.as_deref(),
                model.as_deref(),
                limit,
                include_forgotten,
                since.as_deref(),
                until.as_deref(),
                &now,
            )
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_index_status(
        &self,
        companion: String,
        now: String,
    ) -> crate::ports::Result<memory::IndexStatus> {
        self.call(move |db| memory::index_status(db, &companion, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_skip_consolidation(
        &self,
        companion: String,
        sequence: u64,
        offset: u64,
        method: String,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| {
            memory::skip_consolidation(db, &companion, sequence, offset, &method, &now)
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_retry_index(&self, now: String) -> crate::ports::Result<()> {
        self.call(move |db| memory::retry_index(db, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_record_embedding(
        &self,
        companion: String,
        model: String,
        tokens: u64,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| memory::record_embedding(db, &companion, &model, tokens, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_set_index_state(
        &self,
        companion: String,
        state: String,
        reason: Option<String>,
        model: Option<String>,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| {
            memory::set_index_state(
                db,
                &companion,
                &state,
                reason.as_deref(),
                model.as_deref(),
                &now,
            )
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_consolidation(
        &self,
        companion: String,
        method: String,
        now: String,
    ) -> crate::ports::Result<memory::ConsolidationStatus> {
        self.call(move |db| memory::consolidation_status(db, &companion, &method, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_set_consolidation(
        &self,
        companion: String,
        method: String,
        state: String,
        reason: Option<String>,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| {
            memory::set_consolidation_state(
                db,
                &companion,
                &method,
                &state,
                reason.as_deref(),
                &now,
            )
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_pending_fts(
        &self,
        limit: usize,
        now: String,
    ) -> crate::ports::Result<Vec<String>> {
        self.call(move |db| memory::pending_fts(db, limit, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_index_fts(&self, ids: Vec<String>, now: String) -> crate::ports::Result<()> {
        self.call(move |db| memory::index_fts(db, &ids, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_pending_vectors(
        &self,
        limit: usize,
        now: String,
    ) -> crate::ports::Result<Vec<(String, String, String)>> {
        self.call(move |db| memory::pending_vectors(db, limit, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_store_vectors(
        &self,
        entries: Vec<memory::VectorEntry>,
        model: String,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| memory::store_vectors(db, &entries, &model, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_mark_index_error(
        &self,
        ids: Vec<String>,
        kind: String,
        reason: String,
        permanent: bool,
        now: String,
    ) -> crate::ports::Result<()> {
        self.call(move |db| memory::mark_index_error(db, &ids, &kind, &reason, permanent, &now))
            .await
            .map_err(core_error)
    }
    pub async fn mind_memory_count(&self, companion: String) -> crate::ports::Result<u64> {
        self.call(move |db| {
            let count: i64 = db.query_row(
                "SELECT COUNT(*) FROM mind_memories WHERE owner=?1 OR audience='shared'",
                [companion],
                |row| row.get(0),
            )?;
            Ok(count as u64)
        })
        .await
        .map_err(core_error)
    }
    pub async fn mind_read_visible(
        &self,
        caller: String,
        id: String,
    ) -> crate::ports::Result<Option<crate::domain::mind::MemoryRecord>> {
        self.call(move |db| memory::read_visible(db, &caller, &id))
            .await
            .map_err(core_error)
    }
    pub async fn mind_control(
        &self,
        owner: String,
        id: String,
        action: String,
        now: String,
    ) -> crate::ports::Result<crate::domain::mind::MemoryRecord> {
        self.call(move |db| memory::control(db, &owner, &id, &action, &now))
            .await
            .map_err(core_error)
    }
    #[allow(clippy::too_many_arguments)]
    pub async fn mind_consolidate(
        &self,
        owner: String,
        operation_id: String,
        request_hash: String,
        request: crate::domain::mind::MindUpdateRequest,
        sources: Vec<crate::domain::mind::SourceRef>,
        through_sequence: u64,
        through_offset: u64,
        method: String,
        model_calls: u64,
        input_chars: u64,
        now: String,
    ) -> crate::ports::Result<memory::ConsolidationOutcome> {
        self.call(move |db| {
            memory::consolidate(
                db,
                &owner,
                &operation_id,
                &request_hash,
                &request,
                &sources,
                through_sequence,
                through_offset,
                &method,
                model_calls,
                input_chars,
                &now,
            )
        })
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
        Error::InvalidRequest => CoreError::new(ErrorKind::InvalidRequest),
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
