//! Dispatches character events to plugins and applies validated actions.

use super::{PluginServices, Providers};
use crate::host::{
    client::device,
    db::Store,
    resources::{DeviceHub, HostRuntime, TriggerCharacterEvent},
};
use bevy_ecs::prelude::*;
use ene_plugin::{CharacterAction, CharacterEvent, CharacterTarget, Event, Extension, Operation};
use rig_ecs::prelude::*;
use tokio::sync::mpsc;

/// Dispatches one `CharacterEvent` to all installed `Extension::Character`
/// plugins for `companion_id` and collects validated `CharacterAction`s (`F17`).
pub(crate) async fn dispatch_character_event(
    providers: &Providers,
    store: &Store,
    target: CharacterTarget,
    event: CharacterEvent,
) -> Vec<CharacterAction> {
    let mut actions = Vec::new();
    for plugin_id in providers.with_extension(Extension::Character) {
        let mut collected =
            invoke_character_plugin(providers, store, &plugin_id, &target, &event).await;
        actions.append(&mut collected);
    }
    actions
}

async fn invoke_character_plugin(
    providers: &Providers,
    store: &Store,
    plugin_id: &str,
    target: &CharacterTarget,
    event: &CharacterEvent,
) -> Vec<CharacterAction> {
    let running = match providers.start(plugin_id, Extension::Character).await {
        Ok(running) => running,
        Err(error) => {
            tracing::warn!(plugin = %plugin_id, %error, "character plugin failed to start");
            return Vec::new();
        }
    };
    let services = PluginServices::new(plugin_id.to_owned(), store.clone());
    let (tx, mut rx) = mpsc::channel(32);
    let expected_target = target.clone();
    let collector = tokio::spawn(async move {
        let mut collected = Vec::new();
        while let Some(ev) = rx.recv().await {
            if let Event::Character {
                target: emitted_target,
                action,
            } = ev
                && emitted_target == expected_target
            {
                collected.push(action);
            }
        }
        collected
    });
    if let Err(error) = running
        .invoke_streaming(
            Operation::Character {
                target: target.clone(),
                event: event.clone(),
            },
            services,
            tx,
        )
        .await
    {
        tracing::warn!(plugin = %plugin_id, %error, "character plugin invocation failed");
        return Vec::new();
    }
    collector.await.unwrap_or_default()
}

/// Applies validated `CharacterAction`s emitted by an `Extension::Character`
/// plugin to the companion's voice pipeline or bound client device (`F17`).
pub(crate) fn apply_character_actions(
    world: &mut World,
    companion_id: &str,
    actions: Vec<CharacterAction>,
) {
    if actions.is_empty() {
        return;
    }
    let store = world.resource::<Store>().clone();
    let bound_client = store
        .client_repo()
        .target(companion_id, None)
        .ok()
        .flatten();
    for action in actions {
        match action {
            CharacterAction::Speak { text } => {
                world.trigger(crate::host::resources::SpeakCharacterText {
                    companion_id: companion_id.to_owned(),
                    text,
                });
            }
            CharacterAction::Expression { name, .. } => {
                if let Some(client_id) = bound_client.as_deref()
                    && world.resource::<DeviceHub>().connected(client_id)
                {
                    let input = serde_json::json!({"motion": "idle", "expression": name});
                    let _dispatched = device::dispatch_direct(
                        &device::DeviceDispatchContext {
                            store: &store,
                            hub: world.resource(),
                            notifier: world.resource(),
                        },
                        companion_id,
                        client_id,
                        "body.motion",
                        input,
                    );
                }
            }
            CharacterAction::Motion { asset, .. } => {
                if let Some(client_id) = bound_client.as_deref()
                    && world.resource::<DeviceHub>().connected(client_id)
                {
                    let input = serde_json::json!({"motion": asset});
                    let _dispatched = device::dispatch_direct(
                        &device::DeviceDispatchContext {
                            store: &store,
                            hub: world.resource(),
                            notifier: world.resource(),
                        },
                        companion_id,
                        client_id,
                        "body.motion",
                        input,
                    );
                }
            }
        }
    }
}

/// Pending async character actions ready to be applied on the host ECS thread.
#[derive(Resource, Default)]
struct PendingCharacterActions {
    rx: Option<crossbeam_channel::Receiver<(String, Vec<CharacterAction>)>>,
    tx: Option<crossbeam_channel::Sender<(String, Vec<CharacterAction>)>>,
}

impl PendingCharacterActions {
    fn channel(&mut self) -> crossbeam_channel::Sender<(String, Vec<CharacterAction>)> {
        if let Some(tx) = &self.tx {
            return tx.clone();
        }
        let (tx, rx) = crossbeam_channel::bounded(64);
        self.tx = Some(tx.clone());
        self.rx = Some(rx);
        tx
    }
}

/// Schedules character plugins without blocking the agent loop.
fn observe_character_event(
    event: &TriggerCharacterEvent,
    providers: Option<Res<Providers>>,
    store: Res<Store>,
    runtime: Option<Res<HostRuntime>>,
    wake: Option<Res<Wake>>,
    mut pending: ResMut<PendingCharacterActions>,
) {
    let (Some(providers), Some(runtime)) = (providers, runtime) else {
        return;
    };
    if providers.with_extension(Extension::Character).is_empty() {
        return;
    }
    let store = store.into_inner().clone();
    let device_id = store
        .client_repo()
        .target(&event.companion_id, None)
        .ok()
        .flatten()
        .unwrap_or_else(|| "host".to_owned());
    let cid = event.companion_id.clone();
    let target = CharacterTarget {
        device_id,
        character_id: cid.clone(),
    };
    let providers = providers.into_inner().clone();
    let event = event.event.clone();
    let wake = wake.map(|wake| wake.into_inner().clone());
    let tx = pending.channel();
    runtime.into_inner().0.spawn(async move {
        let actions = dispatch_character_event(&providers, &store, target, event).await;
        if !actions.is_empty() {
            let _queued = tx.try_send((cid, actions));
            if let Some(wake) = wake {
                wake.wake();
            }
        }
    });
}

fn drain_character_actions(world: &mut World) {
    let pending: Vec<(String, Vec<CharacterAction>)> = {
        let Some(res) = world.get_resource::<PendingCharacterActions>() else {
            return;
        };
        let Some(rx) = &res.rx else {
            return;
        };
        rx.try_iter().collect()
    };
    for (companion_id, actions) in pending {
        apply_character_actions(world, &companion_id, actions);
    }
}

/// Handles companion character events and applies plugin actions.
pub(crate) struct CharacterPlugin;

impl Plugin for CharacterPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PendingCharacterActions>()
            .add_observer(
                |trigger: On<TriggerCharacterEvent>,
                 providers: Option<Res<Providers>>,
                 store: Res<Store>,
                 runtime: Option<Res<HostRuntime>>,
                 wake: Option<Res<Wake>>,
                 pending: ResMut<PendingCharacterActions>| {
                    observe_character_event(
                        trigger.event(),
                        providers,
                        store,
                        runtime,
                        wake,
                        pending,
                    );
                },
            )
            .add_systems(Update, drain_character_actions.after(AgentLoop));
    }
}
