//! Connected terminals, exclusive companion activation and device operations.
pub(crate) mod device;

pub(crate) use self::device::{
    DeviceOperation, DeviceResult, DeviceStatus, complete, get_operation, register_tools, restore,
    subscribe, sweep_closed,
};

use super::{
    db::{Result, Store, next_revision, now, validate_id},
    error::HostError,
    notify::Notifier,
    resources::DeviceStreams,
    wire,
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::App;

#[derive(Clone, Debug)]
pub(crate) struct Client {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) platform: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) granted_capabilities: Vec<String>,
    pub(crate) revision: u64,
    pub(crate) last_seen: String,
}

impl Client {
    pub(crate) fn connected(&self, streams: &DeviceStreams) -> bool {
        streams
            .0
            .get(&self.id)
            .is_some_and(|sender| !sender.is_closed())
    }
}

pub(crate) fn register(app: &mut App, request: w::RegisterClientRequest) -> Result<w::Client> {
    validate_id(&request.id)?;
    if request.name.chars().count() > 200 || request.platform.chars().count() > 100 {
        return Err(HostError::invalid("invalid client name"));
    }
    if request
        .capabilities
        .iter()
        .any(|capability| capability.is_empty() || capability.len() > 100)
    {
        return Err(HostError::invalid("invalid capability"));
    }
    let store = app.world().resource::<Store>().clone();
    let existing = store.client(&request.id)?;
    let revision = if let Some(existing) = &existing {
        if existing.revision != request.expected_revision && request.expected_revision != 0 {
            return Err(HostError::conflict("client revision changed"));
        }
        if existing.name == request.name
            && existing.platform == request.platform
            && existing.capabilities == request.capabilities
        {
            existing.revision
        } else {
            next_revision(existing.revision)?
        }
    } else {
        1
    };
    let granted = existing
        .map(|client| {
            client
                .granted_capabilities
                .into_iter()
                .filter(|capability| request.capabilities.contains(capability))
                .collect()
        })
        .unwrap_or_default();
    let client = Client {
        id: request.id,
        name: request.name,
        platform: request.platform,
        capabilities: request.capabilities,
        granted_capabilities: granted,
        revision,
        last_seen: now(),
    };
    store.save_client(&client)?;
    notify(app.world_mut(), &client, false, Vec::new());
    Ok(wire::client(&client, false, Vec::new()))
}

pub(crate) fn list(store: &Store, streams: &DeviceStreams) -> Result<Vec<w::Client>> {
    let activations = store.activations()?;
    Ok(store
        .clients()?
        .iter()
        .map(|client| {
            let active = activations
                .iter()
                .filter(|(_, client_id, _)| client_id == &client.id)
                .map(|(companion, _, _)| companion.clone())
                .collect();
            wire::client(client, client.connected(streams), active)
        })
        .collect())
}

pub(crate) fn set_capabilities(
    app: &mut App,
    request: w::SetCapabilitiesRequest,
) -> Result<w::Client> {
    validate_id(&request.client_id)?;
    let store = app.world().resource::<Store>().clone();
    let mut client = store
        .client(&request.client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    if client.revision != request.revision {
        return Err(HostError::conflict("client revision changed"));
    }
    client.capabilities = request.capabilities;
    client.granted_capabilities = request
        .granted_capabilities
        .into_iter()
        .filter(|capability| client.capabilities.contains(capability))
        .collect();
    client.revision = next_revision(client.revision)?;
    client.last_seen = now();
    store.save_client(&client)?;
    notify(app.world_mut(), &client, false, Vec::new());
    Ok(wire::client(&client, false, Vec::new()))
}

pub(crate) fn activate(app: &mut App, request: w::ActivationRequest) -> Result<w::Activation> {
    validate_id(&request.companion_id)?;
    validate_id(&request.client_id)?;
    let store = app.world().resource::<Store>().clone();
    store
        .companion(&request.companion_id)?
        .ok_or_else(|| HostError::not_found("companion not found"))?;
    let (current_client, current_revision) = store
        .activation(&request.companion_id)?
        .unwrap_or_else(|| (String::new(), 1));
    if current_revision != request.expected_revision {
        return Err(HostError::conflict("activation revision changed"));
    }
    if request.release {
        store.remove_activation(&request.companion_id)?;
        app.world_mut()
            .resource_mut::<super::resources::Perception>()
            .0
            .remove(&request.companion_id);
        notify_activation(app.world_mut(), &request.client_id);
        return Ok(w::Activation {
            companion_id: request.companion_id,
            client_id: None,
            revision: current_revision.saturating_add(1),
            ..Default::default()
        });
    }
    store
        .client(&request.client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    let revision = next_revision(current_revision)?;
    store.save_activation(&request.companion_id, &request.client_id, revision)?;
    app.world_mut()
        .resource_mut::<super::resources::Perception>()
        .0
        .remove(&request.companion_id);
    let _ = current_client;
    notify_activation(app.world_mut(), &request.client_id);
    Ok(w::Activation {
        companion_id: request.companion_id,
        client_id: Some(request.client_id),
        revision,
        ..Default::default()
    })
}

pub(crate) fn activate_on_send(world: &mut World, companion_id: &str, client: &str) -> Result<()> {
    let store = world.resource::<Store>().clone();
    let current = store.activation(companion_id)?;
    if current.as_ref().is_some_and(|(id, _)| id == client) || store.client(client)?.is_none() {
        return Ok(());
    }
    let revision = next_revision(current.map_or(1, |(_, revision)| revision))?;
    store.save_activation(companion_id, client, revision)?;
    world
        .resource_mut::<super::resources::Perception>()
        .0
        .remove(companion_id);
    notify_activation(world, client);
    Ok(())
}

fn notify_activation(world: &mut World, client_id: &str) {
    if let Ok(Some(client)) = world.resource::<Store>().client(client_id) {
        notify(world, &client, false, Vec::new());
    }
}

fn notify(world: &mut World, client: &Client, connected: bool, active: Vec<String>) {
    let view = wire::client(client, connected, active);
    world.resource_mut::<Notifier>().push(
        Some(client.id.clone()),
        None,
        w::notification::Event::ClientUpdated(Box::new(w::ClientUpdated {
            client: view.into(),
            ..Default::default()
        })),
    );
}

pub(crate) fn sweep(store: Res<Store>, mut streams: ResMut<DeviceStreams>, mut commands: Commands) {
    if let Err(error) = sweep_closed(&store, &mut streams)
        && error.is_storage()
    {
        commands.queue(move |world: &mut World| crate::host::fail(world, error.to_string()));
    }
}

pub(crate) fn report_perception(world: &mut World, event: w::PerceptionEvent) -> Result<()> {
    let text = match event.event.as_ref() {
        Some(w::perception_event::Event::PerceptionSummary(summary)) => {
            if summary.summary.trim().is_empty() {
                None
            } else {
                Some(summary.summary.clone())
            }
        }
        Some(w::perception_event::Event::ActiveWindow(window)) => {
            let title = window.title.trim();
            if title.is_empty() {
                None
            } else {
                Some(format!("{} ({})", title, window.app_id))
            }
        }
        Some(w::perception_event::Event::UserActivity(activity)) => Some(if activity.is_active {
            "ユーザーは操作中です".into()
        } else {
            format!("ユーザーは{}秒間無操作です", activity.idle_seconds)
        }),
        None => None,
    };
    if let Some(text) = text {
        let client_id = event.client_id.as_str();
        if !client_id.is_empty() {
            let text = crate::truncate_chars(&text, 1000);
            let companions: Vec<String> = world
                .resource::<Store>()
                .activations()?
                .into_iter()
                .filter(|(_, client, _)| client == client_id)
                .map(|(companion, _, _)| companion)
                .collect();
            let mut perception = world.resource_mut::<super::resources::Perception>();
            for companion in companions {
                perception.0.insert(companion, text.clone());
            }
        }
    }
    Ok(())
}

/// Fire-and-forget host-to-client operation. Used by `eye.watch` and
/// `body.motion`, which do not wait for a device result.
pub(crate) fn send_operation(
    world: &mut World,
    companion: &str,
    capability: &str,
    input: String,
) -> Result<String> {
    let store = world.resource::<Store>().clone();
    let client_id = store
        .activation(companion)?
        .map(|(client, _)| client)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| HostError::conflict("no active client"))?;
    let client = store
        .client(&client_id)?
        .ok_or_else(|| HostError::not_found("client not found"))?;
    if !client
        .granted_capabilities
        .iter()
        .any(|granted| granted == capability)
    {
        return Err(HostError::permission("capability_not_granted"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut record = device::DeviceOperation {
        id: id.clone(),
        client_id: client_id.clone(),
        companion_id: companion.into(),
        capability: capability.into(),
        input: super::db::parse_json(&input)?,
        status: device::DeviceStatus::Pending,
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
            record.status = device::DeviceStatus::Dispatched;
            record.subscription_id = Some(subscription);
            store.save_device(&record)?;
        }
    }
    Ok(id)
}
