//! Per-frame avatar lifecycle: state, visibility, playback, and springs.

use super::assets::ModelAssets;
use super::model::{AvatarState, CharacterId, Config, Notification};
use crate::desktop::Output;
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState},
    prelude::*,
};
use ene_vrm::{SpringRig, Vrm};
use serde_json::json;

/// The single place that decides an avatar's state and applies the transition.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub(super) fn update_avatars(
    mut actors: Query<(
        Entity,
        &CharacterId,
        &Config,
        &mut AvatarState,
        &mut Visibility,
        Option<&ModelAssets>,
    )>,
    outputs: Query<&Output>,
    server: Res<AssetServer>,
    children: Query<&Children>,
    mut parts: Query<(Entity, AnyOf<(&mut AnimationPlayer, &mut SpringRig, &Vrm)>)>,
    mut states: MessageWriter<Notification>,
) {
    use bevy::animation::RepeatAnimation;
    for (root, id, config, mut state, mut visibility, assets) in &mut actors {
        // A scene is ready once its VRM rig exists; the rig is the library's own signal.
        let subtree = assets.map_or(root, |assets| assets.scene);
        let mut initialized = false;
        let mut motion_ready = config.motion.is_none();
        for (_, (player, _, rig)) in
            parts.iter_many(std::iter::once(subtree).chain(children.iter_descendants(subtree)))
        {
            initialized |= rig.is_some();
            motion_ready |=
                player.is_some_and(|player| player.playing_animations().next().is_some());
            if initialized && motion_ready {
                break;
            }
        }
        let mut next = match &*state {
            // A load failure is sticky until the next reload.
            AvatarState::Error(_) => state.clone(),
            _ if !initialized || !motion_ready => AvatarState::Loading,
            _ if outputs.is_empty() => AvatarState::Suspended,
            _ => AvatarState::Ready,
        };
        if *state == AvatarState::Loading {
            let handles = assets.into_iter().flat_map(|a| {
                std::iter::once(a.model.id().untyped())
                    .chain(a.motion.iter().map(|h| h.id().untyped()))
            });
            let paths = std::iter::once(&config.model)
                .chain(config.motion.iter())
                .filter(|_| assets.is_none())
                .filter_map(|p| server.get_path_id(p.clone()));
            for id in handles.chain(paths) {
                if let Some(
                    (LoadState::Failed(error), _, _)
                    | (_, _, RecursiveDependencyLoadState::Failed(error)),
                ) = server.get_load_states(id)
                {
                    next = AvatarState::Error(error.to_string());
                    break;
                }
            }
        }
        visibility.set_if_neq(if next == AvatarState::Ready {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
        if state.set_if_neq(next) {
            let mut event = state.json();
            event["event"] = "state".into();
            event["character"] = id.0.clone().into();
            states.write(Notification(event));
        }
        let running = config.playing && *state == AvatarState::Ready;
        let mut parts =
            parts.iter_many_mut(std::iter::once(root).chain(children.iter_descendants(root)));
        while let Some((_, (player, spring, _))) = parts.fetch_next() {
            if let Some(mut player) = player
                && player.playing_animations().any(|(_, animation)| {
                    animation.is_paused() == running
                        || config.looping != (animation.repeat_mode() == RepeatAnimation::Forever)
                })
            {
                for (_, animation) in player.playing_animations_mut() {
                    if running {
                        animation.resume();
                    } else {
                        animation.pause();
                    }
                    if config.looping {
                        animation.set_repeat(RepeatAnimation::Forever);
                    } else if animation.repeat_mode() == RepeatAnimation::Forever {
                        // Finish the current cycle, even after earlier loop completions.
                        animation.set_repeat(RepeatAnimation::Count(
                            animation.completions().saturating_add(1),
                        ));
                    }
                }
            }
            if let Some(mut spring) = spring {
                // Keep the last simulated pose visible while paused; removing
                // SpringJointProps here would also disable apply_spring_poses.
                let paused = !running;
                if spring.paused != paused {
                    spring.paused = paused;
                }
            }
        }
    }
}

pub(crate) fn notify_finished(
    avatars: Query<(Entity, &CharacterId, &Config, &AvatarState)>,
    players: Query<&AnimationPlayer>,
    children: Query<&Children>,
    mut finished: MessageWriter<Notification>,
) {
    for (entity, id, control, state) in &avatars {
        if !control.playing || control.looping || *state != AvatarState::Ready {
            continue;
        }
        let players =
            || players.iter_many(std::iter::once(entity).chain(children.iter_descendants(entity)));
        if players().all(AnimationPlayer::all_finished)
            && players()
                .flat_map(AnimationPlayer::playing_animations)
                .any(|(_, animation)| !animation.is_paused() && animation.just_completed())
        {
            finished.write(Notification(json!({"event":"finished", "character":id.0})));
        }
    }
}
