//! Per-frame avatar lifecycle: state, visibility, playback, and springs.

use super::assets::ModelAssets;
use super::model::{AvatarState, CharacterId, Config, Notification};
use crate::desktop::Output;
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState, UntypedAssetId},
    prelude::*,
};
use ene_vrm::SpringRig;
use serde_json::json;

fn load_error(
    server: &AssetServer,
    id: UntypedAssetId,
    retrying_dependencies: bool,
) -> Option<String> {
    match server.get_load_states(id) {
        Some((LoadState::Failed(error), _, _)) => Some(error.to_string()),
        Some((_, _, RecursiveDependencyLoadState::Failed(error))) if !retrying_dependencies => {
            Some(error.to_string())
        }
        _ => None,
    }
}

/// The single place that decides an avatar's state and applies the transition.
pub(super) fn update_avatars(
    mut actors: Query<(
        &CharacterId,
        &Config,
        &mut AvatarState,
        &mut Visibility,
        Option<&ModelAssets>,
    )>,
    outputs: Query<&Output>,
    server: Res<AssetServer>,
    mut parts: Query<(&mut AnimationPlayer, Option<&mut SpringRig>)>,
    mut states: MessageWriter<Notification>,
) {
    use bevy::animation::RepeatAnimation;
    for (id, config, mut state, mut visibility, assets) in &mut actors {
        let part = assets
            .and_then(ModelAssets::root)
            .and_then(|root| parts.get_mut(root).ok());
        // Ready requires a bound VRM root, a fully loaded model, and the
        // requested motion wired into that root's player.
        let initialized = part.is_some();
        let model_ready = assets.is_some_and(|assets| {
            !assets.retrying_model_dependencies
                && server.is_loaded_with_dependencies(assets.model.id())
        });
        let motion_ready = config.motion.is_none()
            || part
                .as_ref()
                .is_some_and(|(player, _)| player.playing_animations().next().is_some());
        let mut next = match &*state {
            // A load failure is sticky until the next reload.
            AvatarState::Error(_) => state.clone(),
            _ if !initialized || !model_ready || !motion_ready => AvatarState::Loading,
            _ if outputs.is_empty() => AvatarState::Suspended,
            _ => AvatarState::Ready,
        };
        if !matches!(&*state, AvatarState::Error(_))
            && let Some(assets) = assets
        {
            let model_error = load_error(
                &server,
                assets.model.id().untyped(),
                assets.retrying_model_dependencies,
            );
            let motion_error = assets
                .motion
                .iter()
                .find_map(|handle| load_error(&server, handle.id().untyped(), false));
            if let Some(error) = model_error.or(motion_error) {
                next = AvatarState::Error(error);
            }
        }
        visibility.set_if_neq(if next == AvatarState::Ready {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
        if state.set_if_neq(next) {
            states.write(Notification::state(id, &state));
        }
        let running = config.playing && *state == AvatarState::Ready;
        let Some((mut player, spring)) = part else {
            continue;
        };
        if running {
            player.resume_all();
        } else {
            player.pause_all();
        }
        for (_, animation) in player.playing_animations_mut() {
            if config.looping {
                animation.set_repeat(RepeatAnimation::Forever);
            } else if animation.repeat_mode() == RepeatAnimation::Forever {
                // Finish the current cycle, even after earlier loop completions.
                animation.set_repeat(RepeatAnimation::Count(
                    animation.completions().saturating_add(1),
                ));
            }
        }
        if let Some(mut spring) = spring {
            // Keep the last simulated pose visible while paused; removing
            // SpringJointProps here would also disable apply_spring_poses.
            spring.paused = !running;
        }
    }
}

pub(crate) fn notify_finished(
    avatars: Query<(&CharacterId, &Config, &AvatarState, Option<&ModelAssets>)>,
    players: Query<&AnimationPlayer>,
    mut finished: MessageWriter<Notification>,
) {
    for (id, control, state, assets) in &avatars {
        if !control.playing || control.looping || *state != AvatarState::Ready {
            continue;
        }
        let Some(player) = assets
            .and_then(ModelAssets::root)
            .and_then(|root| players.get(root).ok())
        else {
            continue;
        };
        if player.all_finished()
            && player
                .playing_animations()
                .any(|(_, animation)| !animation.is_paused() && animation.just_completed())
        {
            finished.write(Notification(json!({"event":"finished", "character":id.0})));
        }
    }
}
