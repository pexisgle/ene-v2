//! Device operation streams, non-retrying execution receipts and the `device_request` tool.
use crate::host::{
    Host, Owner,
    db::{Result, aborted, document, invalid, is_storage, not_found, now, permission, validate_id},
    fail, kernel,
    mind::companion,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use uuid::Uuid;

/// A device operation this host is waiting on, and when it stops waiting.
///
/// The turn holds its tool call open until it is answered, so an operation
/// nobody completes would keep the turn running and every later message
/// steered into it.
#[derive(Component, Clone)]
pub(crate) struct PendingDevice {
    pub(crate) operation: String,
    deadline: std::time::Instant,
}

pub(crate) struct DeviceSubscription {
    pub(crate) id: String,
    pub(crate) receiver: mpsc::Receiver<w::DeviceOperation>,
    pub(crate) pending: Vec<w::DeviceOperation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DeviceRecord {
    pub(crate) operation_id: String,
    pub(crate) client_id: String,
    pub(crate) companion_id: String,
    pub(crate) capability: String,
    pub(crate) input: Value,
    pub(crate) status: String,
    pub(crate) result: Option<w::DeviceOperationResult>,
    pub(crate) subscription_id: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

impl DeviceRecord {
    pub(crate) fn state(&self) -> w::DeviceOperationState {
        w::DeviceOperationState {
            operation_id: self.operation_id.clone(),
            client_id: self.client_id.clone(),
            status: self.status.clone(),
            result: self.result.clone().into(),
            ..Default::default()
        }
    }

    pub(crate) fn operation(&self, subscription: &str) -> w::DeviceOperation {
        w::DeviceOperation {
            operation_id: self.operation_id.clone(),
            capability: self.capability.clone(),
            input: document(self.input.clone()).into(),
            subscription_id: subscription.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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

/// Answers a request no terminal replied to in time.
///
/// The answer is `uncertain`, not a failure: the operation may still have
/// run, and retrying it blindly could do the same thing twice.
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
            // Removing it lets `release_device` record the operation as
            // `uncertain`; the call still needs its answer.
            world.entity_mut(call).remove::<PendingDevice>();
            kernel::answer(world, call, Err(aborted("device operation timed out")));
        });
    }
}

pub(crate) fn restore(world: &mut World) -> Result<()> {
    let db = &world.resource::<Host>().db;
    for (key, mut record) in db.list::<DeviceRecord>("device:")? {
        let status = match record.status.as_str() {
            "dispatched" => "uncertain",
            "pending" => "failed",
            _ => continue,
        };
        record.status = status.into();
        record.updated_at = now();
        db.put(&key, &record)?;
    }
    Ok(())
}

pub(crate) fn subscribe(app: &mut App, client: &str) -> Result<DeviceSubscription> {
    validate_id(client)?;
    let host = app.world().resource::<Host>();
    if host
        .db
        .get::<w::Client>(&format!("client:{client}"))?
        .is_none()
    {
        return Err(not_found("client not found"));
    }
    let (sender, receiver) = mpsc::channel(64);
    let id = Uuid::new_v4().to_string();
    let pending = host
        .db
        .list::<DeviceRecord>("device:")?
        .into_iter()
        .filter(|(_, record)| record.client_id == client && record.status == "pending")
        .map(|(key, mut record)| {
            record.status = "dispatched".into();
            record.subscription_id = Some(id.clone());
            record.updated_at = now();
            host.db.put(&key, &record)?;
            Ok(record.operation(&id))
        })
        .collect::<Result<Vec<_>>>()?;
    app.world_mut()
        .resource_mut::<Host>()
        .device_streams
        .insert(client.into(), sender);
    Ok(DeviceSubscription {
        id,
        receiver,
        pending,
    })
}

pub(crate) fn complete(
    app: &mut App,
    result: w::DeviceOperationResult,
) -> Result<w::DeviceOperationState> {
    validate_id(&result.client_id)?;
    validate_id(&result.operation_id)?;
    let key = format!("device:{}", result.operation_id);
    let host = app.world().resource::<Host>();
    let mut record = host
        .db
        .get::<DeviceRecord>(&key)?
        .ok_or_else(|| not_found("operation not found"))?;
    if record.client_id != result.client_id {
        return Err(not_found("operation not found"));
    }
    if record.status == "completed" {
        return Ok(record.state());
    }
    if record.status != "dispatched" && record.status != "uncertain" {
        return Err(aborted("operation is not waiting"));
    }
    record.status = if result.success {
        "completed"
    } else {
        "failed"
    }
    .into();
    record.result = Some(result.clone());
    record.updated_at = now();
    host.db.put(&key, &record)?;
    let waiting = app
        .world_mut()
        .query::<(Entity, &PendingDevice)>()
        .iter(app.world())
        .find_map(|(call, pending)| (pending.operation == result.operation_id).then_some(call));
    if let Some(call) = waiting {
        app.world_mut().entity_mut(call).remove::<PendingDevice>();
        let text = if result.success {
            result
                .output
                .as_option()
                .map(|doc| doc.json.clone())
                .unwrap_or_default()
        } else {
            result
                .failure
                .as_option()
                .map(|f| f.message.clone())
                .unwrap_or_else(|| "device operation failed".into())
        };
        kernel::answer(
            app.world_mut(),
            call,
            if result.success {
                Ok(text)
            } else {
                Err(aborted("device operation failed"))
            },
        );
    }
    Ok(record.state())
}

pub(crate) fn get_operation(app: &mut App, id: &str) -> Result<w::DeviceOperationState> {
    validate_id(id)?;
    app.world()
        .resource::<Host>()
        .db
        .get::<DeviceRecord>(&format!("device:{id}"))?
        .ok_or_else(|| not_found("operation not found"))
        .map(|record| record.state())
}

/// Marks the operations of a terminal that went away as `uncertain`.
pub(crate) fn sweep_closed(host: &mut Host) -> Result<()> {
    let closed: Vec<String> = host
        .device_streams
        .iter()
        .filter(|(_, sender)| sender.is_closed())
        .map(|(client, _)| client.clone())
        .collect();
    if !closed.is_empty() {
        for client in &closed {
            for (key, mut record) in host.db.list::<DeviceRecord>("device:")? {
                if record.client_id == *client && record.status == "dispatched" {
                    record.status = "uncertain".into();
                    record.updated_at = now();
                    host.db.put(&key, &record)?;
                }
            }
        }
        host.device_streams
            .retain(|client, _| !closed.contains(client));
    }
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
        let key = format!("device:{}", entry.operation);
        let result = {
            let host = world.resource::<Host>();
            match host.db.get::<DeviceRecord>(&key) {
                Ok(Some(mut record))
                    if record.status == "pending" || record.status == "dispatched" =>
                {
                    record.status = "uncertain".into();
                    record.updated_at = now();
                    host.db.put(&key, &record)
                }
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            }
        };
        if let Err(error) = result
            && is_storage(&error)
        {
            fail(world, error.to_string());
        }
    });
}

fn device_requested(called: On<ToolCalled<DeviceRequestArgs>>, mut commands: Commands) {
    let event = called.event();
    let (call, agent, args) = (event.call, event.agent, event.args.clone());
    commands.queue(move |world: &mut World| {
        if let Err(error) = request_device(world, agent, call, args) {
            kernel::answer(world, call, Err(error));
        }
    });
}

fn request_device(
    world: &mut World,
    agent: Entity,
    call: Entity,
    args: DeviceRequestArgs,
) -> Result<()> {
    let owner = world
        .get::<Owner>(agent)
        .cloned()
        .ok_or_else(|| permission("unbound agent"))?;
    if args.capability.is_empty() || args.capability.len() > 100 {
        return Err(invalid("invalid capability"));
    }
    let host = world.resource::<Host>();
    let deadline = std::time::Instant::now() + host.device_timeout;
    let companion = companion(&host.db, owner.companion())?;
    // Only the companion's active terminal: a model does not choose one and
    // so cannot step around the exclusive activation.
    let client_id = companion
        .companion
        .active_client
        .filter(|id| !id.is_empty())
        .ok_or_else(|| aborted("no active client"))?;
    let client = host
        .db
        .get::<w::Client>(&format!("client:{client_id}"))?
        .ok_or_else(|| not_found("client not found"))?;
    if !client
        .granted_capabilities
        .iter()
        .any(|c| c == &args.capability)
    {
        return Err(permission("capability_not_granted"));
    }
    let id = Uuid::new_v4().to_string();
    let mut record = DeviceRecord {
        operation_id: id.clone(),
        client_id: client_id.clone(),
        companion_id: owner.companion().into(),
        capability: args.capability,
        input: args.input,
        status: "pending".into(),
        result: None,
        subscription_id: None,
        created_at: now(),
        updated_at: now(),
    };
    // Recorded before it is sent: a terminal that goes away must be left
    // `pending`, not an operation nobody knows about.
    host.db.put(&format!("device:{id}"), &record)?;
    if let Some(sender) = host.device_streams.get(&client_id) {
        let subscription = format!("{client_id}:{id}");
        if sender.try_send(record.operation(&subscription)).is_ok() {
            record.status = "dispatched".into();
            record.subscription_id = Some(subscription);
            host.db.put(&format!("device:{id}"), &record)?;
        }
    }
    world.entity_mut(call).insert(PendingDevice {
        operation: id,
        deadline,
    });
    Ok(())
}
