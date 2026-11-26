//! Writing, delivering and replaying device operations.
//!
//! The operation is written first and delivered second. A client with no live
//! stream leaves it `pending` for the reconnect, and a replay never repeats a
//! Computer Use operation.

use super::{
    CapabilitySpec, DeviceOperation, DeviceStatus,
    computer::{self, ComputerUse},
};
use crate::host::{
    db::{Result, Store},
    error::HostError,
    notify::Notifier,
    permission::{self, CallBinding},
    resources::{CompanionAgent, DeviceHub, WorkAgent},
    wire,
};
use bevy_ecs::prelude::*;
use ene_domain::{Client, CompanionId, DeviceScope, Failure};
use ene_rpc as w;
use ene_store::util::now;
use rig_ecs::model::Connection;
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(crate) enum Invocation<'a> {
    Request(&'a str),
    Capability(&'a CapabilitySpec),
}

/// Writes and dispatches one operation, returning its id and the immediate
/// tool answer. The answer reports the status dispatch reached: `dispatched`
/// when the client stream accepted the operation, `pending` when no live
/// stream has it yet. It never reports completion.
/// The operation goes to the client the gate bound the call to; a client that
/// moved since then does not void it.
pub(crate) fn accept(
    world: &mut World,
    call: Entity,
    agent: Entity,
    invocation: Invocation<'_>,
    input: Value,
) -> Result<(String, String)> {
    let store = world.resource::<Store>();
    let binding = world
        .get::<CallBinding>(call)
        .ok_or_else(|| HostError::Internal(("a device call without a binding").into()))?;
    let scope = agent_scope(world, agent)?;
    if scope.run_id() != binding.work_run.as_deref() {
        return Err(HostError::Conflict(
            "the accepted operation belongs to a different work run".into(),
        ));
    }
    let client = store
        .client_repo()
        .client(&binding.client_id)?
        .ok_or_else(|| HostError::Conflict(("the bound client is no longer registered").into()))?;
    if !client
        .capabilities
        .iter()
        .any(|name| name == &binding.capability)
    {
        return Err(HostError::Conflict(
            ("the capability is no longer declared").into(),
        ));
    }
    if !client.is_granted(&binding.capability) {
        return Err(HostError::Conflict(
            ("the capability is no longer granted").into(),
        ));
    }
    let spec = match invocation {
        Invocation::Request(capability) if capability == binding.capability => {
            client.spec(capability)
        }
        Invocation::Capability(spec) if spec.name == binding.capability => Some(spec),
        _ => {
            return Err(HostError::Conflict(
                "the capability no longer matches the accepted operation".into(),
            ));
        }
    };
    if let Some(spec) = spec {
        super::validate_input(spec, &input)?;
    }
    let action = permission::CandidateAction::DeviceCapability {
        capability: binding.capability.clone(),
        spec: spec.cloned().map(Box::new),
        tool_name: binding.capability.clone(),
        input: input.clone(),
        raw_arguments: input.clone(),
    };
    let typed_target = permission::extract_target(&action);
    if let Some(reason) = permission::hard_denied_action(&action, &typed_target) {
        return Err(HostError::Permission(reason));
    }
    let computer = if binding.capability == w::COMPUTER_USE {
        let work = world.get::<WorkAgent>(agent).ok_or_else(|| {
            HostError::Permission(("Computer Use requires a work session").into())
        })?;
        let run = scope
            .run_id()
            .ok_or_else(|| HostError::Internal(("Computer Use has no bound run").into()))?;
        if spec.is_none() || !computer::model_supported(world.get::<Connection>(agent)) {
            return Err(HostError::Permission(
                ("Computer Use requires a declared schema and image capable task model").into(),
            ));
        }
        Some(computer::prepare(
            &computer::ComputerState {
                store: world.resource::<Store>(),
                hub: world.resource::<DeviceHub>(),
                computer: world.resource::<computer::ComputerUse>(),
            },
            work,
            &binding.client_id,
            run,
            &input,
        )?)
    } else {
        None
    };
    // The work session is recorded with the operation; the dispatch target is
    // the bound client.
    let operation_id = uuid::Uuid::new_v4().to_string();
    let operation = DeviceOperation {
        id: operation_id.clone(),
        client_id: binding.client_id.clone(),
        capability: binding.capability.clone(),
        input,
        scope,
        status: DeviceStatus::Pending,
        output: None,
        failure: None,
        created_at: now(),
        updated_at: now(),
    };
    store.client_repo().insert_operation(&operation)?;
    let status = {
        let ctx = DeviceDispatchContext {
            store,
            hub: world.resource::<DeviceHub>(),
            notifier: world.resource::<Notifier>(),
        };
        dispatch(&ctx, &operation, computer.as_ref())?
    };
    if let Some(computer) = computer {
        if status != DeviceStatus::Dispatched {
            store
                .client_repo()
                .set_operation_status(&operation.id, DeviceStatus::Failed)?;
            return Err(HostError::Conflict(
                ("the Computer Use device stream is unavailable").into(),
            ));
        }
        let mut state = world.resource_mut::<ComputerUse>();
        state.frames.remove(&operation.client_id);
        state.operations.insert(operation_id.clone(), computer);
    }
    let answer = json!({"operation_id": &operation_id, "status": status.as_str()}).to_string();
    Ok((operation_id, answer))
}

/// Dispatches an authorized host-initiated capability operation (such as an
/// `Extension::Character` expression/motion action) directly to `client_id`
/// when the client has declared and been granted `capability`.
pub(crate) fn dispatch_direct(
    ctx: &DeviceDispatchContext<'_>,
    companion_id: &str,
    client_id: &str,
    capability: &str,
    input: Value,
) -> Result<String> {
    let store = ctx.store;
    let client = store
        .client_repo()
        .client(client_id)?
        .ok_or_else(|| HostError::NotFound("client not found".into()))?;
    if !client.is_granted(capability) {
        return Err(HostError::Permission(format!(
            "capability `{capability}` is not granted on `{client_id}`"
        )));
    }
    let operation_id = uuid::Uuid::new_v4().to_string();
    let operation = DeviceOperation {
        id: operation_id.clone(),
        client_id: client_id.to_owned(),
        capability: capability.to_owned(),
        input,
        scope: DeviceScope::Companion(CompanionId::parse(companion_id)?),
        status: DeviceStatus::Pending,
        output: None,
        failure: None,
        created_at: now(),
        updated_at: now(),
    };
    store.client_repo().insert_operation(&operation)?;
    let _reply = dispatch(ctx, &operation, None)?;
    Ok(operation_id)
}

fn agent_scope(world: &World, agent: Entity) -> Result<DeviceScope> {
    if let Some(companion) = world.get::<CompanionAgent>(agent) {
        return Ok(DeviceScope::Companion(CompanionId::parse(
            &companion.companion_id,
        )?));
    }
    let work = world
        .get::<WorkAgent>(agent)
        .ok_or_else(|| HostError::Internal("a device call without an agent".into()))?;
    Ok(DeviceScope::Work {
        companion_id: CompanionId::parse(&work.companion_id)?,
        work_id: work.work_id.clone(),
        run_id: work.run_id.clone(),
    })
}

pub(crate) struct DeviceDispatchContext<'a> {
    pub(crate) store: &'a Store,
    pub(crate) hub: &'a DeviceHub,
    pub(crate) notifier: &'a Notifier,
}

/// Writes the operation to the client's live stream. A missing stream leaves
/// the operation pending for the reconnect and is reported as such; a sent
/// operation is `dispatched`.
fn dispatch(
    ctx: &DeviceDispatchContext<'_>,
    operation: &DeviceOperation,
    computer: Option<&computer::Pending>,
) -> Result<DeviceStatus> {
    let Some((subscription, permit)) = ctx.hub.reserve(&operation.client_id) else {
        return Ok(DeviceStatus::Pending);
    };
    if computer.is_some_and(|pending| pending.subscription != subscription) {
        return Ok(DeviceStatus::Pending);
    }
    let wire_operation = wire::device_operation(operation, &subscription.to_string());
    // Persist the claim before exposing the event. A crash or stream closure
    // after this point leaves an uncertain dispatch instead of a replayable one.
    if !ctx
        .store
        .client_repo()
        .mark_operation_dispatched(&operation.id)?
    {
        return Err(HostError::Conflict(
            "the device operation is no longer pending".into(),
        ));
    }
    let _sender = permit.send(w::DeviceEvent {
        event: Some(w::device_event::Event::Operation(Box::new(
            wire_operation.clone(),
        ))),
        ..Default::default()
    });
    ctx.notifier.push(
        None,
        Some(&operation.client_id),
        w::notification::Event::DeviceOperation(Box::new(wire_operation)),
    );
    Ok(DeviceStatus::Dispatched)
}

fn scope_is_live(store: &Store, operation: &DeviceOperation) -> Result<bool> {
    let target = Some(operation.client_id.as_str());
    Ok(match &operation.scope {
        DeviceScope::Companion(id) => store
            .client_repo()
            .activation(id.as_str())?
            .is_some_and(|activation| activation.client_id.as_deref() == target),
        DeviceScope::Work {
            companion_id,
            work_id,
            run_id,
        } => {
            let repo = store.work_repo();
            store
                .companion_repo()
                .companion(companion_id.as_str())?
                .is_some()
                && repo.work_session(work_id)?.is_some_and(|work| {
                    work.companion_id == *companion_id
                        && work.client_id.as_deref() == target
                        && work.status != ene_domain::WorkStatus::Closed
                })
                && repo
                    .active_run(work_id)?
                    .is_some_and(|run| run.run_id == *run_id)
        }
        DeviceScope::Legacy { .. } => false,
    })
}

fn capability_still_authorized(
    rules: &[permission::PolicyRule],
    client: Option<&Client>,
    operation: &DeviceOperation,
) -> bool {
    let Some(client) = client.filter(|client| client.is_granted(&operation.capability)) else {
        return false;
    };
    let spec = client.spec(&operation.capability);
    if spec.is_some_and(|spec| super::validate_input(spec, &operation.input).is_err()) {
        return false;
    }
    let arguments = json!({"capability": operation.capability, "input": operation.input});
    let verdict = ene_policy::decide_capability(
        rules,
        spec,
        &operation.capability,
        "device_request",
        &arguments,
    );
    !matches!(verdict, ene_policy::Verdict::Deny { .. })
}

/// Replays operations that were never delivered after a client reconnects.
pub(crate) fn resend(
    ctx: &DeviceDispatchContext<'_>,
    client: &str,
    mut reject: impl FnMut(&str, Option<&Failure>),
) -> Result<()> {
    let repo = ctx.store.client_repo();
    let mut pending = repo
        .outstanding_operations(client)?
        .into_iter()
        .filter(|operation| operation.status == DeviceStatus::Pending)
        .peekable();
    if pending.peek().is_none() {
        return Ok(());
    }
    let declaration = repo.client(client)?;
    let rules = ctx.store.policy_repo().policy_rules()?;
    for operation in pending {
        if operation.capability == w::COMPUTER_USE {
            reject(&operation.id, None);
        } else if scope_is_live(ctx.store, &operation)?
            && capability_still_authorized(&rules, declaration.as_ref(), &operation)
        {
            let _status = dispatch(ctx, &operation, None)?;
        } else {
            let failure = Failure::new(
                "operation_authority_changed",
                "the operation's owner or permission changed before it was dispatched",
            );
            let _saved = repo.complete_operation(&operation.id, Err(&failure))?;
            reject(&operation.id, Some(&failure));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_requires_capacity_and_claims_each_pending_row_once() {
        let state = tempfile::tempdir().unwrap();
        let store = Store::open(&state.path().join("host.sqlite3")).unwrap();
        let repo = store.client_repo();
        let hub = DeviceHub::default();
        let (subscription, mut receiver) =
            hub.register(&ene_domain::ClientId::parse("desk").unwrap());
        let operation = DeviceOperation {
            id: "motion".into(),
            client_id: "desk".into(),
            capability: "body.motion".into(),
            input: json!({"motion": "nod"}),
            scope: DeviceScope::Companion(CompanionId::parse("ene").unwrap()),
            status: DeviceStatus::Pending,
            output: None,
            failure: None,
            created_at: String::new(),
            updated_at: String::new(),
        };
        repo.insert_operation(&operation).unwrap();
        let context = DeviceDispatchContext {
            store: &store,
            hub: &hub,
            notifier: &Notifier::new(),
        };
        let send = || dispatch(&context, &operation, None);
        let reserved: Vec<_> = (0..receiver.max_capacity())
            .map(|_| hub.reserve("desk").unwrap())
            .collect();
        assert_eq!(send().unwrap(), DeviceStatus::Pending);
        assert_eq!(
            repo.operation("motion").unwrap().unwrap().status,
            DeviceStatus::Pending
        );
        drop(reserved);

        assert_eq!(send().unwrap(), DeviceStatus::Dispatched);
        assert_eq!(
            repo.operation("motion").unwrap().unwrap().status,
            DeviceStatus::Dispatched
        );
        assert!(matches!(send(), Err(HostError::Conflict(_))));
        let Some(w::device_event::Event::Operation(sent)) = receiver.try_recv().unwrap().event
        else {
            panic!("the device did not receive the operation");
        };
        assert!(subscription.matches_wire(&sent.subscription_id));
        assert_eq!(sent.operation_id, "motion");
        assert!(receiver.try_recv().is_err());
    }
}
