//! Accepts a work input once across both executors and host restarts.
use super::{ReadyWork, RunState, WorkStatus, executor_of, new_run, require_session};
use crate::host::{
    db::{Result, Store},
    error::HostError,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use ene_store::work::WorkInputReceipt;

pub(crate) fn send(world: &mut World, request: &w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let repo = store.work_repo();
    let session = require_session(&store, &request.work_id)?;
    let hash = ene_store::util::digest(format!("{}:{}", session.id, request.text).as_bytes());
    let key = request.request_key.as_str();
    if !key.is_empty()
        && let Some(previous) = repo.input_receipt(&session.id, key)?
    {
        if previous.payload_hash != hash {
            return Err(HostError::AlreadyExists(
                "this request key already accepted different content".into(),
            ));
        }
        return Ok(w::WorkAcceptance {
            request_id: previous.request_id,
            run_id: Some(previous.run_id),
            ..Default::default()
        });
    }
    if session.status == WorkStatus::Closed {
        return Err(HostError::Conflict("work session is closed".into()));
    }
    if request.text.trim().is_empty() {
        return Err(HostError::Invalid("an instruction is required".into()));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    let receipt = (!key.is_empty()).then_some(WorkInputReceipt {
        request_key: key,
        payload_hash: &hash,
        request_id: &request_id,
    });
    let executor = executor_of(&store, &session, world.get_resource())?.recorded();
    let run = new_run(
        uuid::Uuid::new_v4().to_string(),
        &session.id,
        &request.text,
        RunState::Starting,
    );
    repo.insert_run(&run, &executor, receipt)?;
    world.resource_mut::<ReadyWork>().0.insert(session.id);
    Ok(w::WorkAcceptance {
        request_id,
        run_id: Some(run.run_id),
        ..Default::default()
    })
}
