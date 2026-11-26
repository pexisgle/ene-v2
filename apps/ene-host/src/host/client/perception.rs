//! Perception events: observations routed to the eyes, and the legacy passive
//! summary fallback.

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

/// Routes live observations to the eyes and supports legacy passive summaries.
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
        if matches!(
            &event.event,
            Some(w::perception_event::Event::WatchMatch(_))
        ) && !client.is_granted("eye.watch")
        {
            return Ok(());
        }
        if hub.into_inner().subscription(&event.client_id).as_deref()
            == Some(event.subscription_id.as_str())
        {
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
    let Some(summary) = perception_summary(event) else {
        return Ok(());
    };
    let avatar_interaction = match &event.event {
        Some(w::perception_event::Event::AvatarInteraction(interaction)) => Some((
            format!("{:?}", interaction.kind),
            interaction.character_id.clone(),
            interaction.region.clone(),
        )),
        _ => None,
    };
    for companion_id in &targets {
        perception.events.insert(
            companion_id.clone(),
            PerceptionEvent {
                summary: summary.clone(),
            },
        );
    }
    if let Some((kind, character_id, region)) = avatar_interaction {
        for companion_id in targets {
            commands.trigger(TriggerCharacterEvent {
                companion_id,
                event: ene_plugin::CharacterEvent::Custom {
                    name: "avatar_interaction".into(),
                    data: serde_json::json!({
                        "kind": kind,
                        "character_id": character_id,
                        "region": region,
                    }),
                },
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn report_perception(world: &mut World, event: &w::PerceptionEvent) -> Result<()> {
    world
        .run_system_cached_with(ingest_perception, event)
        .unwrap()
}

fn perception_summary(event: &w::PerceptionEvent) -> Option<String> {
    let summary = match event.event.as_ref()? {
        w::perception_event::Event::ActiveWindow(active) => {
            if active.title.is_empty() && active.app_id.is_empty() {
                return None;
            }
            format!("アクティブウィンドウ: {} ({})", active.title, active.app_id)
        }
        w::perception_event::Event::UserActivity(state) => {
            if state.is_active {
                "ユーザーは操作中です".to_owned()
            } else {
                format!("ユーザーは離席中です (idle {} 秒)", state.idle_seconds)
            }
        }
        w::perception_event::Event::PerceptionSummary(summary) => {
            if summary.summary.trim().is_empty() {
                return None;
            }
            summary.summary.clone()
        }
        w::perception_event::Event::AvatarInteraction(interaction) => {
            let kind_label = match interaction.kind.as_known() {
                Some(w::AvatarInteractionKind::DoubleClick) => "ダブルクリック",
                Some(w::AvatarInteractionKind::Drag) => "ドラッグ",
                _ => "クリック",
            };
            if interaction.region.trim().is_empty() {
                format!("ユーザーがアバターを{kind_label}しました")
            } else {
                format!(
                    "ユーザーがアバター({})を{kind_label}しました",
                    interaction.region.trim()
                )
            }
        }
        _ => return None,
    };
    Some(util::truncate(&summary, 1_000))
}
