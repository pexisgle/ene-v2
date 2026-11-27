//! Accepts a work input once across both executors and host restarts.
use super::{ReadyWork, RunState, WorkStatus, agent_of, executor_of, new_run, require_session};
use crate::host::{
    db::{Result, Store},
    error::HostError,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use ene_store::work::{AcceptedWorkInput, WorkInputReceipt};
use rig_ecs::prelude::*;

pub(crate) fn send(world: &mut World, request: &w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let repo = store.work_repo();
    let session = require_session(&store, &request.work_id)?;
    let hash = ene_store::util::digest(format!("{}:{}", session.id, request.text).as_bytes());
    let key = request.request_key.as_str();
    if !key.is_empty() {
        let saved = repo.input_receipt(&session.id, key)?;
        let previous = saved.or_else(|| {
            let agent = agent_of(world, &session.id)?;
            legacy_receipt(world.get::<Conversation>(agent), key)
        });
        if let Some(previous) = previous {
            if previous.payload_hash != hash {
                return Err(HostError::AlreadyExists(
                    "this request key already accepted different content".into(),
                ));
            }
            return Ok(w::WorkAcceptance {
                request_id: previous.request_id,
                run_id: previous.run_id,
                ..Default::default()
            });
        }
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

fn legacy_receipt(conversation: Option<&Conversation>, key: &str) -> Option<AcceptedWorkInput> {
    conversation?.asides().find_map(|(_, aside)| {
        if aside.kind != "ene.work_accepted" || aside.value["request_key"].as_str()? != key {
            return None;
        }
        let bytes = aside.value["hash"]
            .as_array()?
            .iter()
            .map(|byte| u8::try_from(byte.as_u64()?).ok())
            .collect::<Option<Vec<_>>>()?;
        let id = aside.value["request_id"].as_str()?;
        Some(AcceptedWorkInput {
            payload_hash: bytes,
            request_id: id.to_owned(),
            run_id: None,
        })
    })
}
