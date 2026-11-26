//! Keeping an agent's model connection current.
//!
//! The store is the source of truth and the binding is derived: a cached
//! connection is dropped when the settings revision, the credential generation
//! or the model factory changes, and rebuilt from the companion's assignment.

use crate::host::{
    db::{Result, Store},
    error::HostError,
    mind::Role,
    model::{Credentials, ModelFactory},
    notify::Notifier,
    resources::{CompanionAgent, WorkAgent},
};
use bevy_ecs::prelude::*;
use rig_core::conversation::Author;
use rig_ecs::model::{Connection as ModelConnection, Models};
use rig_ecs::prelude::*;
use std::collections::HashMap;

#[derive(Resource, Default)]
pub(crate) struct Connections {
    revision: u64,
    credential_generation: u64,
    factory: Option<ModelFactory>,
    models: HashMap<(String, String), ModelConnection>,
}

impl Connections {
    pub(crate) fn clear_models(&mut self) {
        self.models.clear();
    }
}

/// Binds the agent's model when its assignment or the settings changed.
#[tracing::instrument(
    name = "agent.ensure_model",
    skip_all,
    fields(?entity, companion_id = tracing::field::Empty)
)]
pub(crate) fn ensure_model(world: &mut World, entity: Entity) -> Result<()> {
    world.init_resource::<Connections>();
    let (companion_id, role) = match world.get::<CompanionAgent>(entity) {
        Some(agent) => (agent.companion_id.clone(), Role::Companion),
        None => match world.get::<WorkAgent>(entity) {
            Some(agent) => (agent.companion_id.clone(), Role::Task),
            None => return Err(HostError::Internal(("an agent without a role").into())),
        },
    };
    tracing::Span::current().record("companion_id", companion_id.as_str());
    let store = world.resource::<Store>().clone();
    let companion = store
        .companion_repo()
        .companion(&companion_id)?
        .ok_or_else(|| HostError::NotFound(("companion not found").into()))?;
    let assignment = companion.model(role).map(|assignment| {
        (
            assignment.connection_id.clone(),
            assignment.model_id.clone(),
        )
    });
    let revision = store.settings_repo().settings_revision()?;
    let credentials = world.resource::<Credentials>().clone();
    let credential_generation = credentials.generation();
    let factory = world.resource::<ModelFactory>().clone();
    {
        let mut cache = world.resource_mut::<Connections>();
        if cache.revision != revision
            || cache.credential_generation != credential_generation
            || cache
                .factory
                .as_ref()
                .is_none_or(|previous| !std::sync::Arc::ptr_eq(&previous.0, &factory.0))
        {
            cache.models.clear();
            cache.revision = revision;
            cache.credential_generation = credential_generation;
            cache.factory = Some(factory.clone());
        }
    }
    let cached = assignment
        .as_ref()
        .and_then(|key| world.resource::<Connections>().models.get(key).cloned());
    let connection = match cached {
        Some(connection) => connection,
        None => {
            let catalog = world.resource::<Models>().0.catalog().clone();
            let providers = world
                .get_resource::<crate::host::providers::Providers>()
                .cloned()
                .unwrap_or_default();
            let model = (factory.0)(&store, &credentials, &catalog, &companion, role, &providers)?;
            let spec = std::sync::Arc::new(model.spec().cloned().ok_or_else(|| {
                HostError::Internal(("a completion model without catalog facts").into())
            })?);
            let handler =
                rig_core::serve::ErasedHandler::new(rig_core::serve::adapters::ModelAdapter::<
                    rig_core::operation::Completion,
                >::new(spec.reference(), model));
            let connection = ModelConnection {
                spec,
                handler: rig_ecs::effects::Handler(handler),
            };
            if let Some(key) = assignment {
                world
                    .resource_mut::<Connections>()
                    .models
                    .insert(key, connection.clone());
            }
            connection
        }
    };
    world.entity_mut(entity).insert(connection);
    Ok(())
}

/// Tells the clients that a saved companion has no usable model yet.
pub(super) fn notice_model_unavailable(
    notifier: &mut Notifier,
    companion_id: &str,
    error: &HostError,
) {
    notifier.notice(
        Some(companion_id),
        "model_unavailable",
        &ene_store::util::truncate(&error.to_string(), 300),
    );
}

/// Hands a turn to one agent, but only when its model is bound. Voice and the
/// schedule loop deliver turns without going through `chat`, which is where a
/// typed message asks for a model; without this a turn can run on the model the
/// agent was bound to before its assignment or settings changed.
#[tracing::instrument(name = "agent.deliver", skip_all, fields(companion_id = %companion_id, ?mode))]
pub(crate) fn deliver(
    world: &mut World,
    entity: Entity,
    companion_id: &str,
    text: String,
    mode: DeliveryMode,
    author: Author,
) -> bool {
    if let Err(error) = ensure_model(world, entity) {
        notice_model_unavailable(&mut world.resource_mut::<Notifier>(), companion_id, &error);
        return false;
    }
    world.trigger(Deliver::new(entity, text, mode).with_author(author));
    true
}
