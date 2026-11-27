//! Clip playback for a character: the pending change, the crossfade, and the
//! systems that apply them.

use super::{AvatarState, Character, Notification, load_error, load_motion};
use bevy::{animation::graph::AnimationNodeType, prelude::*};
use ene_avatar_ipc::AvatarEvent;

const MOTION_FADE_SECS: f32 = 0.3;

pub(super) enum Pending {
    Initial(u64),
    Change(MotionChange),
}

pub(super) struct MotionChange {
    path: Option<String>,
    looping: bool,
    request: Option<u64>,
    phase: ClipChange,
}

enum ClipChange {
    Loading(Handle<AnimationClip>),
    // Bevy threads a newly assigned graph leaf on the next frame.
    Attaching {
        clip: Handle<AnimationClip>,
        node: AnimationNodeIndex,
    },
    Rest,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MotionFailure {
    #[error("the animation asset could not be loaded")]
    Asset,
    #[error("a newer operation superseded this motion")]
    Superseded,
    #[error("the renderer is shutting down")]
    ShuttingDown,
}

#[derive(Message)]
pub(crate) struct MotionOutcome {
    pub(crate) request: u64,
    pub(crate) result: Result<(), MotionFailure>,
}

impl Pending {
    pub(super) fn change(
        server: &AssetServer,
        path: Option<String>,
        looping: bool,
        request: Option<u64>,
    ) -> Self {
        let phase = path.as_ref().map_or(ClipChange::Rest, |path| {
            ClipChange::Loading(load_motion(server, path))
        });
        Self::Change(MotionChange {
            path,
            looping,
            request,
            phase,
        })
    }

    pub(super) fn rejected(&self, reason: MotionFailure) -> Option<MotionOutcome> {
        let request = match self {
            Self::Initial(request) => Some(*request),
            Self::Change(change) => change.request,
        }?;
        Some(MotionOutcome {
            request,
            result: Err(reason),
        })
    }
}

pub(super) struct FadingMotion {
    node: AnimationNodeIndex,
    start_weight: f32,
    elapsed: f32,
}

impl Character {
    pub(super) fn play_motion(&mut self, player: &mut AnimationPlayer, node: AnimationNodeIndex) {
        if !self.config.playing || self.state != AvatarState::Ready {
            player.stop_all();
            self.fading.clear();
        } else if let Some(previous) = self.main
            && let Some(animation) = player.animation(previous)
        {
            let weight = animation.weight();
            if weight > 0. {
                self.fading.push(FadingMotion {
                    node: previous,
                    start_weight: weight,
                    elapsed: 0.,
                });
            } else {
                player.stop(previous);
            }
        }
        self.main = Some(node);
        let start_weight = if self.fading.is_empty() { 1. } else { 0. };
        let animation = player.start(node).set_weight(start_weight);
        if self.config.looping {
            animation.repeat();
        }
        if !self.config.playing || self.state != AvatarState::Ready {
            animation.pause();
        }
    }

    pub(super) fn advance_fades(&mut self, player: &mut AnimationPlayer, delta: f32) {
        // Each outgoing clip fades independently from its visible weight.
        // Interrupting again leaves all earlier fades on their original clocks.
        let mut outgoing = 0.;
        self.fading.retain_mut(|fade| {
            fade.elapsed += delta;
            if fade.elapsed >= MOTION_FADE_SECS {
                player.stop(fade.node);
                return false;
            }
            if let Some(animation) = player.animation_mut(fade.node) {
                let t = fade.elapsed / MOTION_FADE_SECS;
                let weight = fade.start_weight * (1. - t * t * (3. - 2. * t));
                animation.set_weight(weight);
                outgoing += weight;
            }
            true
        });
        if let Some(animation) = self.main.and_then(|node| player.animation_mut(node)) {
            animation.set_weight((1. - outgoing).max(0.));
        }
    }
}

/// Applies a motion change once its clip is loaded, fading over 300 ms instead
/// of rebuilding the character.
pub(super) fn motion_change(
    server: Res<AssetServer>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut characters: Query<&mut Character>,
    mut roots: Query<(&mut AnimationPlayer, &AnimationGraphHandle)>,
    mut outcomes: MessageWriter<MotionOutcome>,
) {
    let server = server.into_inner();
    for mut c in &mut characters {
        let Some(pending) = c.pending.take() else {
            continue;
        };
        let (request, result) = match pending {
            Pending::Initial(request) => match &c.state {
                AvatarState::Loading => {
                    c.pending = Some(Pending::Initial(request));
                    continue;
                }
                AvatarState::Ready | AvatarState::Suspended => (Some(request), Ok(())),
                AvatarState::Error(_) => (Some(request), Err(MotionFailure::Asset)),
            },
            Pending::Change(mut change) => {
                match change.advance(server, &mut graphs, &mut roots, &mut c) {
                    Some(result) => (change.request, result),
                    None => {
                        c.pending = Some(Pending::Change(change));
                        continue;
                    }
                }
            }
        };
        if let Some(request) = request {
            outcomes.write(MotionOutcome { request, result });
        }
    }
}

impl MotionChange {
    fn advance(
        &mut self,
        server: &AssetServer,
        graphs: &mut Assets<AnimationGraph>,
        roots: &mut Query<(&mut AnimationPlayer, &AnimationGraphHandle)>,
        c: &mut Character,
    ) -> Option<Result<(), MotionFailure>> {
        if matches!(c.state, AvatarState::Error(_)) {
            return Some(Err(MotionFailure::Asset));
        }
        if let ClipChange::Loading(clip) = &self.phase
            && let Some(error) = load_error(server, clip.id().untyped(), false)
        {
            warn!(character = %c.id, %error, "the motion did not load");
            return Some(Err(MotionFailure::Asset));
        }
        let (mut player, handle) = roots.get_mut(c.root?).ok()?;
        let playing = match &self.phase {
            ClipChange::Rest => None,
            ClipChange::Loading(clip) => {
                if !server.is_loaded_with_dependencies(clip.id()) {
                    return None;
                }
                let (node, rebuild) = motion_node(graphs, &handle.0, &player, clip)?;
                if rebuild {
                    self.phase = ClipChange::Attaching {
                        clip: clip.clone(),
                        node,
                    };
                    return None;
                }
                Some((clip.clone(), node))
            }
            ClipChange::Attaching { clip, node } => Some((clip.clone(), *node)),
        };
        c.config.motion = self.path.take();
        c.config.looping = self.looping;
        if let Some((clip, node)) = playing {
            c.play_motion(&mut player, node);
            c.motion = Some(clip);
        } else {
            player.stop_all();
            c.motion = None;
            c.main = None;
            c.fading.clear();
            if let Some(mut graph) = graphs.get_mut(&handle.0)
                && graph.nodes().count() > 1
            {
                *graph = AnimationGraph::new();
            }
        }
        Some(Ok(()))
    }
}

pub(super) fn motion_node(
    graphs: &mut Assets<AnimationGraph>,
    handle: &Handle<AnimationGraph>,
    player: &AnimationPlayer,
    clip: &Handle<AnimationClip>,
) -> Option<(AnimationNodeIndex, bool)> {
    // An active instance may still supply the outgoing pose of a replayed clip.
    let graph = graphs.get(handle)?;
    let mut idle = None;
    for node in graph
        .nodes()
        .filter(|&node| !player.is_playing_animation(node))
    {
        if let AnimationNodeType::Clip(previous) = &graph.get(node)?.node_type {
            if previous == clip {
                return Some((node, false));
            }
            idle.get_or_insert(node);
        }
    }
    let mut graph = graphs.get_mut(handle)?;
    let node = if let Some(node) = idle {
        graph.get_mut(node)?.node_type = AnimationNodeType::Clip(clip.clone());
        node
    } else {
        let root = graph.root;
        graph.add_clip(clip.clone(), 1., root)
    };
    Some((node, true))
}

pub(super) fn advance_fades(
    time: Res<Time>,
    mut characters: Query<&mut Character>,
    mut players: Query<&mut AnimationPlayer>,
) {
    let delta = time.into_inner().delta_secs();
    for mut c in &mut characters {
        if c.config.playing
            && c.state == AvatarState::Ready
            && !c.fading.is_empty()
            && let Some(mut player) = c.root.and_then(|root| players.get_mut(root).ok())
        {
            c.advance_fades(&mut player, delta);
        }
    }
}

pub fn notify_finished(
    characters: Query<&Character>,
    players: Query<&AnimationPlayer>,
    mut events: MessageWriter<Notification>,
) {
    for c in &characters {
        if c.config.playing
            && !c.config.looping
            && c.state == AvatarState::Ready
            && let Some(animation) = c
                .main
                .and_then(|node| players.get(c.root?).ok()?.animation(node))
            && !animation.is_paused()
            && animation.is_finished()
            && animation.just_completed()
        {
            events.write(Notification(AvatarEvent::Finished {
                character: c.id.clone(),
            }));
        }
    }
}
