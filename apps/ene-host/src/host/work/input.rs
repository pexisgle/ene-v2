//! Accepts a work input once across both executors and host restarts.
use super::{
    Runs, WorkAgent, WorkStatus, agent_of, bind_work_agent, hand, require_session, start_run,
};
use crate::host::{
    db::{Result, Store},
    error::HostError,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use ene_store::work::WorkInputReceipt;
use rig_ecs::prelude::*;

pub(crate) fn send(world: &mut World, request: &w::SendWorkRequest) -> Result<w::WorkAcceptance> {
    let store = world.resource::<Store>().clone();
    let repo = store.work_repo();
    let session = require_session(&store, &request.work_id)?;
    let agent = agent_of(world, &session.id);
    let hash = ene_store::util::digest(format!("{}:{}", session.id, request.text).as_bytes());
    let key = request.request_key.as_str();
    if !key.is_empty() {
        let saved = repo.input_receipt(&session.id, key)?;
        let previous = saved.or_else(|| {
            legacy_receipt(
                agent.and_then(|entity| world.get::<Conversation>(entity)),
                key,
            )
        });
        if let Some((payload, request_id)) = previous {
            if payload != hash {
                return Err(HostError::AlreadyExists(
                    "this request key already accepted different content".into(),
                ));
            }
            return Ok(w::WorkAcceptance {
                request_id,
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
    if let Some(run) = repo.active_run(&session.id)? {
        let is_harness = hand::owns(world.resource::<hand::Hands>(), &session.id);
        if !is_harness && agent.is_none() {
            return Err(HostError::Unavailable(
                "the work agent is not running".into(),
            ));
        }
        if let Some(receipt) = receipt {
            repo.record_input(&session.id, receipt)?;
        }
        if is_harness {
            hand::steer(
                &mut world.resource_mut::<hand::Hands>(),
                &session.id,
                &request.text,
            );
        } else if let Some(agent) = agent {
            bind_work_agent(world.get_mut::<WorkAgent>(agent), &run.run_id);
            let author = Author {
                request: Some(RequestId(request_id.clone())),
                ..Author::user()
            };
            world.trigger(
                Deliver::new(agent, request.text.clone(), DeliveryMode::Queue).with_author(author),
            );
        }
        world.resource_mut::<Runs>().touch(&run.run_id);
    } else {
        start_run(world, &session, &request.text, receipt)?;
    }
    Ok(w::WorkAcceptance {
        request_id,
        ..Default::default()
    })
}

fn legacy_receipt(conversation: Option<&Conversation>, key: &str) -> Option<(Vec<u8>, String)> {
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
        Some((bytes, id.to_owned()))
    })
}
