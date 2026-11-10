//! Client device registration, capability grants and exclusive companion activation.
mod device;

pub(crate) use self::device::{
    complete, get_operation, register_tools, restore, subscribe, sweep_closed,
};

use super::{
    Host,
    db::{Result, aborted, invalid, next_revision, not_found, now, validate_id},
    mind::{CompanionRecord, companion},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use rig_ecs::prelude::*;

pub(crate) fn register(app: &mut App, request: w::RegisterClientRequest) -> Result<w::Client> {
    validate_id(&request.id)?;
    if request.name.chars().count() > 200 || request.platform.chars().count() > 100 {
        return Err(invalid("invalid client name"));
    }
    if request
        .capabilities
        .iter()
        .any(|c| c.is_empty() || c.len() > 100)
    {
        return Err(invalid("invalid capability"));
    }
    let key = format!("client:{}", request.id);
    let host = app.world().resource::<Host>();
    let existing = host.db.get::<w::Client>(&key)?;
    let revision = if let Some(existing) = &existing {
        if existing.revision != request.expected_revision && request.expected_revision != 0 {
            return Err(aborted("client revision changed"));
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
        .map(|c| {
            c.granted_capabilities
                .into_iter()
                .filter(|cap| request.capabilities.contains(cap))
                .collect()
        })
        .unwrap_or_default();
    let client = w::Client {
        id: request.id,
        name: request.name,
        platform: request.platform,
        capabilities: request.capabilities,
        granted_capabilities: granted,
        revision,
        last_seen: now(),
        connected: false,
        active_companions: Vec::new(),
        ..Default::default()
    };
    host.db.put(&key, &client)?;
    notify(app.world_mut(), &client);
    Ok(client)
}

pub(crate) fn list(host: &Host) -> Result<Vec<w::Client>> {
    let companions = host.db.list::<CompanionRecord>("companion:")?;
    let mut clients: Vec<w::Client> = host
        .db
        .list::<w::Client>("client:")?
        .into_iter()
        .map(|(_, client)| client)
        .collect();
    for client in &mut clients {
        client.connected = host
            .device_streams
            .get(&client.id)
            .is_some_and(|s| !s.is_closed());
        client.active_companions = companions
            .iter()
            .filter(|(_, record)| record.companion.active_client.as_deref() == Some(&client.id))
            .map(|(_, record)| record.companion.id.clone())
            .collect();
    }
    Ok(clients)
}

pub(crate) fn set_capabilities(
    app: &mut App,
    request: w::SetCapabilitiesRequest,
) -> Result<w::Client> {
    validate_id(&request.client_id)?;
    let key = format!("client:{}", request.client_id);
    let host = app.world().resource::<Host>();
    let mut client = host
        .db
        .get::<w::Client>(&key)?
        .ok_or_else(|| not_found("client not found"))?;
    if client.revision != request.revision {
        return Err(aborted("client revision changed"));
    }
    client.capabilities = request.capabilities;
    client.granted_capabilities = request
        .granted_capabilities
        .into_iter()
        .filter(|cap| client.capabilities.contains(cap))
        .collect();
    client.revision = next_revision(client.revision)?;
    client.last_seen = now();
    host.db.put(&key, &client)?;
    notify(app.world_mut(), &client);
    Ok(client)
}

pub(crate) fn activate(app: &mut App, request: w::ActivationRequest) -> Result<w::Activation> {
    validate_id(&request.companion_id)?;
    validate_id(&request.client_id)?;
    let host = app.world().resource::<Host>();
    let mut record = companion(&host.db, &request.companion_id)?;
    if record.companion.activation_revision != request.expected_revision {
        return Err(aborted("activation revision changed"));
    }
    if !request.release
        && host
            .db
            .get::<w::Client>(&format!("client:{}", request.client_id))?
            .is_none()
    {
        return Err(not_found("client not found"));
    }
    let active = (!request.release).then(|| request.client_id.clone());
    apply_activation(
        app.world_mut(),
        &mut record,
        active,
        Some(request.client_id),
    )
}

pub(crate) fn activate_on_send(world: &mut World, companion_id: &str, client: &str) -> Result<()> {
    let host = world.resource::<Host>();
    let mut record = companion(&host.db, companion_id)?;
    if record.companion.active_client.as_deref() == Some(client)
        || host
            .db
            .get::<w::Client>(&format!("client:{client}"))?
            .is_none()
    {
        return Ok(());
    }
    apply_activation(world, &mut record, Some(client.into()), None)?;
    Ok(())
}

fn apply_activation(
    world: &mut World,
    record: &mut CompanionRecord,
    active_client: Option<String>,
    notify_client: Option<String>,
) -> Result<w::Activation> {
    let revision = next_revision(record.companion.activation_revision)?;
    record.companion.activation_revision = revision;
    record.companion.active_client.clone_from(&active_client);
    record.companion.updated_at = now();
    world
        .resource::<Host>()
        .db
        .put(&format!("companion:{}", record.companion.id), record)?;
    world.resource_mut::<Host>().notify(
        Some(record.companion.id.clone()),
        notify_client,
        w::notification::Event::ActivationChanged(Box::new(w::ActivationChanged {
            companion_id: record.companion.id.clone(),
            client_id: active_client.clone(),
            revision,
            ..Default::default()
        })),
    );
    Ok(w::Activation {
        companion_id: record.companion.id.clone(),
        client_id: active_client,
        revision,
        ..Default::default()
    })
}

fn notify(world: &mut World, client: &w::Client) {
    world.resource_mut::<Host>().notify(
        None,
        Some(client.id.clone()),
        w::notification::Event::ClientUpdated(Box::new(w::ClientUpdated {
            client: client.clone().into(),
            ..Default::default()
        })),
    );
}
