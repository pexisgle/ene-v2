//! Device operations, their streams and non-retrying execution receipts.
use crate::host::{
    agent::AgentKey,
    db::{Result, Store, now, validate_id},
    error::HostError,
    failure::Failure,
    resources::{DeviceStreams, DeviceTimeout},
    tools, wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceStatus {
    Pending,
    Dispatched,
    Completed,
    Failed,
    Uncertain,
}

impl DeviceStatus {
    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "dispatched" => Self::Dispatched,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Uncertain,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatched => "dispatched",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DeviceResult {
    pub(crate) success: bool,
    pub(crate) output: Value,
    pub(crate) failure: Option<Failure>,
}

#[derive(Clone, Debug)]
pub(crate) struct DeviceOperation {
    pub(crate) id: String,
    pub(crate) client_id: String,
    pub(crate) companion_id: String,
    pub(crate) capability: String,
    pub(crate) input: Value,
    pub(crate) status: DeviceStatus,
    pub(crate) result: Option<DeviceResult>,
    pub(crate) subscription_id: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug)]
pub(crate) struct DeviceSubscription {
    pub(crate) id: String,
    pub(crate) receiver: mpsc::Receiver<w::DeviceOperation>,
    pub(crate) pending: Vec<w::DeviceOperation>,
}

#[derive(Component, Clone)]
pub(crate) struct PendingDevice {
    pub(crate) operation: String,
    deadline: std::time::Instant,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DeviceRequestArgs {
    capability: String,
    input: Value,
}

pub(crate) fn register_tools(app: &mut App) {
    app.add_observer(release_device)
        .add_systems(Update, expire)
        .add_open_tool::<DeviceRequestArgs, _>(
            "device_request",
            "Run an operation on the active client device, such as a file or screen capability. The result arrives with a later model call.",
            ToolOptions::default(),
            device_requested,
        );
}

fn expire(pending: Query<(Entity, &PendingDevice)>, mut commands: Commands) {
    let now = std::time::Instant::now();
    for (call, entry) in &pending {
        if now < entry.deadline {
            continue;
        }
        commands.queue(move |world: &mut World| {
            if world.get::<PendingDevice>(call).is_none() {
                return;
            }
            world.entity_mut(call).remove::<PendingDevice>();
            tools::answer(
                world,
                call,
                Err(HostError::conflict("device operation timed out")),
            );
        });
    }
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let store = world.resource::<Store>().clone();
    for mut record in store.devices()? {
        let status = match record.status {
            DeviceStatus::Dispatched => DeviceStatus::Uncertain,
            DeviceStatus::Pending => DeviceStatus::Failed,
            _ => continue,
        };
        record.status = status;
        record.updated_at = now();
        store.save_device(&record)?;
    }
    Ok(())
}

pub(crate) fn subscribe(world: &mut World, client: &str) -> Result<DeviceSubscription> {
    validate_id(client)?;
    let store = world.resource::<Store>().clone();
    if store.client(client)?.is_none() {
        return Err(HostError::not_found("client not found"));
    }
    let (sender, receiver) = mpsc::channel(64);
    let id = Uuid::new_v4().to_string();
    let mut pending = Vec::new();
    for mut record in store.pending_devices(client)? {
        record.status = DeviceStatus::Dispatched;
        record.subscription_id = Some(id.clone());
        record.updated_at = now();
        store.save_device(&record)?;
        pending.push(wire::device_task(&record, &id));
    }
    world
        .resource_mut::<DeviceStreams>()
        .0
        .insert(client.into(), sender);
    Ok(DeviceSubscription {
        id,
        receiver,
        pending,
    })
}

pub(crate) fn complete(
    world: &mut World,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    validate_id(&result.client_id)?;
    validate_id(&result.operation_id)?;
    let store = world.resource::<Store>().clone();
    let mut record = store
        .device(&result.operation_id)?
        .ok_or_else(|| HostError::not_found("operation not found"))?;
    if record.client_id != result.client_id {
        return Err(HostError::not_found("operation not found"));
    }
    if record.status == DeviceStatus::Completed {
        return Ok(wire::device_state(&record));
    }
    if record.status != DeviceStatus::Dispatched && record.status != DeviceStatus::Uncertain {
        return Err(HostError::conflict("operation is not waiting"));
    }
    record.status = if result.success {
        DeviceStatus::Completed
    } else {
        DeviceStatus::Failed
    };
    record.result = Some(DeviceResult {
        success: result.success,
        output: wire::parse(&result.output)?,
        failure: result.failure.as_option().map(|failure| Failure {
            code: failure.code.clone(),
            message: failure.message.clone(),
        }),
    });
    record.updated_at = now();
    store.save_device(&record)?;
    let waiting = world
        .query::<(Entity, &PendingDevice)>()
        .iter(world)
        .find_map(|(call, pending)| (pending.operation == result.operation_id).then_some(call));
    if let Some(call) = waiting {
        world.entity_mut(call).remove::<PendingDevice>();
        let text = if result.success {
            record
                .result
                .as_ref()
                .map(|value| wire::json(&value.output))
                .unwrap_or_default()
        } else {
            record
                .result
                .as_ref()
                .and_then(|value| value.failure.as_ref())
                .map(|failure| failure.message.clone())
                .unwrap_or_else(|| "device operation failed".into())
        };
        tools::answer(
            world,
            call,
            if result.success {
                Ok(text)
            } else {
                Err(HostError::conflict("device operation failed"))
            },
        );
    }
    Ok(wire::device_state(&record))
}

pub(crate) fn get_operation(world: &mut World, id: &str) -> Result<w::DeviceOperationState> {
    validate_id(id)?;
    let store = world.resource::<Store>().clone();
    store
        .device(id)?
        .ok_or_else(|| HostError::not_found("operation not found"))
        .map(|record| wire::device_state(&record))
}

/// Marks operations of terminals that went away as `uncertain`.
pub(crate) fn sweep_closed(store: &Store, streams: &mut DeviceStreams) -> Result<()> {
    let closed: Vec<String> = streams
        .0
        .iter()
        .filter(|(_, sender)| sender.is_closed())
        .map(|(client, _)| client.clone())
        .collect();
    if closed.is_empty() {
        return Ok(());
    }
    for record in store.devices()? {
        if !closed.contains(&record.client_id) || record.status != DeviceStatus::Dispatched {
            continue;
        }
        let mut record = record;
        record.status = DeviceStatus::Uncertain;
        record.updated_at = now();
        store.save_device(&record)?;
    }
    streams.0.retain(|client, _| !closed.contains(client));
    Ok(())
}

fn release_device(
    removed: On<Remove<PendingDevice>>,
    pending: Query<&PendingDevice>,
    mut commands: Commands,
) {
    let Ok(entry) = pending.get(removed.entity).cloned() else {
        return;
    };
    commands.queue(move |world: &mut World| {
        let store = world.resource::<Store>().clone();
        let Ok(Some(mut record)) = store.device(&entry.operation) else {
            return;
        };
        if record.status == DeviceStatus::Pending || record.status == DeviceStatus::Dispatched {
            record.status = DeviceStatus::Uncertain;
            record.updated_at = now();
            if let Err(error) = store.save_device(&record)
                && error.is_storage()
            {
                crate::host::fail(world, error.to_string());
            }
        }
    });
}

fn device_requested(called: On<ToolCalled<DeviceRequestArgs>>, mut commands: Commands) {
    let event = called.event();
    let (call, agent, args) = (event.call, event.agent, event.args.clone());
    commands.queue(move |world: &mut World| {
        if let Err(error) = request_device(world, agent, call, args) {
            tools::answer(world, call, Err(error));
        }
    });
}

fn request_device(
    world: &mut World,
    agent: Entity,
    call: Entity,
    args: DeviceRequestArgs,
) -> Result<()> {
    let key = world
        .get::<AgentKey>(agent)
        .cloned()
        .ok_or_else(|| HostError::permission("unbound agent"))?;
    if args.capability.is_empty() || args.capability.len() > 100 {
        return Err(HostError::invalid("invalid capability"));
    }
    let store = world.resource::<Store>().clone();
    store
        .companion(key.companion())?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let client_id = store
        .activation(key.companion())?
        .map(|(client, _)| client)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| HostError::conflict("no active client"))?;
    let client = store
        .client(&client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    if !client
        .granted_capabilities
        .iter()
        .any(|capability| capability == &args.capability)
    {
        return Err(HostError::permission("capability_not_granted"));
    }
    let deadline = std::time::Instant::now() + world.resource::<DeviceTimeout>().0;
    let id = Uuid::new_v4().to_string();
    let mut record = DeviceOperation {
        id: id.clone(),
        client_id: client_id.clone(),
        companion_id: key.companion().into(),
        capability: args.capability,
        input: args.input,
        status: DeviceStatus::Pending,
        result: None,
        subscription_id: None,
        created_at: now(),
        updated_at: now(),
    };
    store.save_device(&record)?;
    if let Some(sender) = world.resource::<DeviceStreams>().0.get(&client_id) {
        let subscription = format!("{client_id}:{id}");
        if sender
            .try_send(wire::device_task(&record, &subscription))
            .is_ok()
        {
            record.status = DeviceStatus::Dispatched;
            record.subscription_id = Some(subscription);
            store.save_device(&record)?;
        }
    }
    world.entity_mut(call).insert(PendingDevice {
        operation: id,
        deadline,
    });
    Ok(())
}
