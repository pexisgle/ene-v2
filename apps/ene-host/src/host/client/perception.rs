//! Routes consented sensor data to Eyes and direct clicks to the avatar.

use super::super::error::{HostError, Result};
use super::super::{
    db::Store,
    eyes::{self, Eyes},
    notify::Notifier,
    resources::{DeviceHub, Perception, PerceptionEvent, TriggerCharacterEvent},
};
use bevy_ecs::{prelude::*, system::SystemParam};
use ene_rpc as w;
use ene_store::util;

#[derive(SystemParam)]
pub(crate) struct PerceptionInputs<'w> {
    store: Res<'w, Store>,
    hub: Res<'w, DeviceHub>,
    eyes: ResMut<'w, Eyes>,
    perception: ResMut<'w, Perception>,
    notifier: Option<Res<'w, Notifier>>,
}

/// Routes live observations and direct avatar interactions.
pub(crate) fn ingest_perception(
    InRef(event): InRef<w::PerceptionEvent>,
    inputs: PerceptionInputs<'_>,
    mut commands: Commands,
) -> Result<()> {
    let PerceptionInputs {
        store,
        hub,
        mut eyes,
        mut perception,
        notifier,
    } = inputs;
    use w::perception_event::Event;

    let store = store.into_inner();
    ene_domain::validate_id(&event.client_id)?;
    let client = store
        .client_repo()
        .client(&event.client_id)?
        .ok_or_else(|| HostError::NotFound(("client not found").into()))?;
    let active_companions = store
        .client_repo()
        .active_companions_for_client(&event.client_id)?;
    if active_companions.is_empty() {
        // A connected client that is not active produces nothing the head may
        // act on. Not an error: the event is simply discarded.
        return Ok(());
    }
    let targets: Vec<String> = match event.companion_id.as_deref().filter(|id| !id.is_empty()) {
        Some(target) => {
            if !active_companions.iter().any(|id| id == target) {
                return Ok(());
            }
            vec![target.to_owned()]
        }
        None => active_companions,
    };
    if !event.subscription_id.is_empty() {
        if matches!(&event.event, Some(Event::WatchMatch(_))) && !client.is_granted("eye.watch") {
            return Ok(());
        }
        let consent = match &event.event {
            Some(Event::Enabled(enabled)) => Some(*enabled),
            _ => None,
        };
        if hub.into_inner().admit_eyes(
            &event.client_id,
            &event.subscription_id,
            event.consent_generation.unwrap_or_default(),
            consent,
        ) {
            let notifier = notifier.map(Res::into_inner);
            for companion_id in targets {
                eyes::apply_report(
                    &mut eyes,
                    &mut perception,
                    notifier,
                    &companion_id,
                    event.clone(),
                )?;
            }
        }
        return Ok(());
    }
    let Some(Event::AvatarInteraction(interaction)) = &event.event else {
        return Ok(());
    };
    let summary = avatar_summary(interaction);
    for companion_id in targets {
        perception.events.insert(
            companion_id.clone(),
            PerceptionEvent {
                summary: summary.clone(),
            },
        );
        commands.trigger(TriggerCharacterEvent {
            companion_id,
            event: ene_plugin::CharacterEvent::Custom {
                name: "avatar_interaction".into(),
                data: serde_json::json!({
                    "kind": format!("{:?}", interaction.kind),
                    "character_id": interaction.character_id,
                    "region": interaction.region,
                }),
            },
        });
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn report_perception(world: &mut World, event: &w::PerceptionEvent) -> Result<()> {
    world
        .run_system_cached_with(ingest_perception, event)
        .unwrap()
}

fn avatar_summary(interaction: &w::AvatarInteraction) -> String {
    let action = match interaction.kind.as_known() {
        Some(w::AvatarInteractionKind::DoubleClick) => "ダブルクリック",
        Some(w::AvatarInteractionKind::Drag) => "ドラッグ",
        _ => "クリック",
    };
    let summary = if interaction.region.trim().is_empty() {
        format!("ユーザーがアバターを{action}しました")
    } else {
        format!(
            "ユーザーがアバター({})を{action}しました",
            interaction.region.trim()
        )
    };
    util::truncate(&summary, 1_000)
}
