mod motion;

pub use motion::notify_finished;
use motion::{FadingMotion, Pending, advance_fades, motion_change};

use crate::desktop::Output;
use bevy::{
    animation::RepeatAnimation,
    app::AnimationSystems,
    asset::{LoadState, RecursiveDependencyLoadState, UntypedAssetId},
    camera::visibility::VisibilitySystems,
    gltf::{Gltf, GltfAssetLabel},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use ene_vrm::{ExpressionWeights, GazeOverride, SpringRig, Vrm, VrmSystems};
use serde_json::{Value, json};

#[derive(Component, Default)]
#[require(Transform, Visibility)]
pub struct Character {
    pub id: String,
    pub config: Config,
    pub state: AvatarState,
    pub root: Option<Entity>,
    model: Handle<Gltf>,
    motion: Option<Handle<AnimationClip>>,
    /// A motion the renderer is waiting to load before it fades to it.
    pending: Option<Pending>,
    /// The playback node for the latest motion, separate from fading clips.
    main: Option<AnimationNodeIndex>,
    fading: Vec<FadingMotion>,
    /// The expression the client asked for, and how far the fade has got.
    pub expression: Option<String>,
    pub expression_weight: f32,
    /// The mouth opening the client asked for (from playback amplitude), and the
    /// smoothed value the blendshape actually uses.
    pub lip: f32,
    pub lip_level: f32,
    /// Optional direct eye gaze angles `(yaw_deg, pitch_deg)` overriding clip look-at.
    pub gaze: Option<(f32, f32)>,
    spawned: bool,
    retrying: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AvatarState {
    #[default]
    Loading,
    Ready,
    Suspended,
    Error(String),
}

impl AvatarState {
    pub fn json(&self) -> Value {
        let (status, error) = match self {
            Self::Loading => ("loading", None),
            Self::Ready => ("ready", None),
            Self::Suspended => ("suspended", None),
            Self::Error(error) => ("error", Some(error)),
        };
        json!({"status":status, "error":error})
    }
}

pub use ene_avatar_ipc::CharacterConfig as Config;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0}")]
    AssetPath(String),
    #[error("invalid size or position")]
    InvalidSizeOrPosition,
    #[error("expected .vrm or .vrma path")]
    InvalidExtension,
}

pub fn validate_config(config: &Config) -> Result<(), ConfigError> {
    for path in std::iter::once(&config.model).chain(config.motion.iter()) {
        bevy::asset::AssetPath::try_parse(path)
            .map_err(|error| ConfigError::AssetPath(error.to_string()))?;
    }
    if !(50. ..=200.).contains(&config.size_percent)
        || config
            .position
            .is_some_and(|p| p.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.))
    {
        return Err(ConfigError::InvalidSizeOrPosition);
    }
    if !config.model.ends_with(".vrm")
        || config
            .motion
            .as_ref()
            .is_some_and(|path| !path.ends_with(".vrma"))
    {
        return Err(ConfigError::InvalidExtension);
    }
    Ok(())
}

#[derive(Message)]
pub struct Notification(pub Value);

impl Character {
    pub fn new(id: String, config: Config) -> Self {
        Self {
            id,
            config,
            ..default()
        }
    }

    fn notification(&self) -> Notification {
        let mut event = self.state.json();
        event["event"] = "state".into();
        event["character"] = self.id.clone().into();
        Notification(event)
    }
}

pub fn setup(app: &mut App) {
    app.add_message::<Notification>()
        .add_observer(|event: On<WorldInstanceReady>, params: SceneReadyParams| {
            scene_ready(event.entity, params);
        })
        .add_systems(
            PostUpdate,
            (update, advance_fades, motion_change)
                .chain()
                .before(AnimationSystems)
                .before(VisibilitySystems::VisibilityPropagate),
        )
        .add_systems(PostUpdate, notify_finished.after(AnimationSystems))
        // Sync direct gaze overrides before VRM look-at computes eye bone / morph targets.
        .add_systems(
            PostUpdate,
            sync_gaze.after(AnimationSystems).before(VrmSystems::LookAt),
        )
        // The mouth and the face are written after the animation reset of the
        // frame, and before ene-vrm turns weights into morph values.
        .add_systems(
            PostUpdate,
            expressions
                .after(AnimationSystems)
                .before(VrmSystems::Expressions),
        )
        .configure_sets(
            PostUpdate,
            VrmSystems::SpringBone.before(VisibilitySystems::CalculateBounds),
        );
}

pub fn reload(world: &mut World, entity: Entity) {
    let Some(character) = world.get::<Character>(entity) else {
        return;
    };
    let server = world.resource::<AssetServer>();
    // Bevy denies paths outside its approved roots. The client selected these
    // files explicitly, so approve exactly these two loads and nothing else.
    let model = server
        .load_builder()
        .override_unapproved()
        .load(character.config.model.clone());
    let motion = character
        .config
        .motion
        .as_ref()
        .map(|path| load_motion(server, path));
    // Loaded owners must be reloaded to retry failed textures and other dependencies.
    let retrying = matches!(
        server.get_load_states(model.id()),
        Some((
            LoadState::Loaded,
            _,
            RecursiveDependencyLoadState::Failed(_)
        ))
    );
    if retrying {
        server.reload(character.config.model.clone());
    }
    world
        .entity_mut(entity)
        .despawn_children()
        .insert(Visibility::Hidden);
    let Some(mut character) = world.get_mut::<Character>(entity) else {
        return;
    };
    character.model = model;
    character.motion = motion;
    character.pending = None;
    character.main = None;
    character.fading.clear();
    character.root = None;
    character.spawned = false;
    character.retrying = retrying;
    character.state = AvatarState::Loading;
    let event = character.notification();
    world.write_message(event);
}

fn load_motion(server: &AssetServer, path: &str) -> Handle<AnimationClip> {
    server
        .load_builder()
        .override_unapproved()
        .load(GltfAssetLabel::Animation(0).from_asset(path.to_owned()))
}

pub fn queue_motion(world: &mut World, entity: Entity) {
    let Some(path) = world
        .get::<Character>(entity)
        .and_then(|character| character.config.motion.as_deref())
    else {
        if let Some(mut character) = world.get_mut::<Character>(entity) {
            character.pending = Some(Pending::Rest);
        }
        return;
    };
    let pending = Pending::Fade(load_motion(world.resource::<AssetServer>(), path));
    if let Some(mut character) = world.get_mut::<Character>(entity) {
        character.pending = Some(pending);
    }
}

#[derive(bevy::ecs::system::SystemParam)]
struct SceneReadyParams<'w, 's> {
    parents: Query<'w, 's, &'static ChildOf>,
    children: Query<'w, 's, &'static Children>,
    characters: Query<'w, 's, &'static mut Character>,
    players: Query<'w, 's, &'static mut AnimationPlayer, With<Vrm>>,
    graphs: ResMut<'w, Assets<AnimationGraph>>,
    commands: Commands<'w, 's>,
}

fn scene_ready(instance: Entity, mut params: SceneReadyParams) {
    let Ok(parent) = params.parents.get(instance) else {
        return;
    };
    let Ok(mut character) = params.characters.get_mut(parent.parent()) else {
        return;
    };
    let Some(root) = params
        .children
        .iter_descendants(instance)
        .find(|e| params.players.contains(*e))
    else {
        return;
    };
    character.root = Some(root);
    let graph = if let Some(clip) = &character.motion
        && let Ok(mut player) = params.players.get_mut(root)
    {
        let (graph, node) = AnimationGraph::from_clip(clip.clone());
        character.play_motion(&mut player, node);
        graph
    } else {
        AnimationGraph::new()
    };
    params
        .commands
        .entity(root)
        .insert(AnimationGraphHandle(params.graphs.add(graph)));
}

fn load_error(server: &AssetServer, id: UntypedAssetId, retrying: bool) -> Option<String> {
    match server.get_load_states(id) {
        Some((LoadState::Failed(error), _, _)) => Some(error.to_string()),
        Some((_, _, RecursiveDependencyLoadState::Failed(error))) if !retrying => {
            Some(error.to_string())
        }
        _ => None,
    }
}

#[derive(bevy::ecs::system::SystemParam)]
struct UpdateCharactersParams<'w, 's> {
    characters: Query<'w, 's, (Entity, &'static mut Character, &'static mut Visibility)>,
    outputs: Query<'w, 's, &'static Output>,
    server: Res<'w, AssetServer>,
    models: Res<'w, Assets<Gltf>>,
    modified: MessageReader<'w, 's, AssetEvent<Gltf>>,
    players: Query<'w, 's, (&'static mut AnimationPlayer, Option<&'static mut SpringRig>)>,
    events: MessageWriter<'w, Notification>,
    commands: Commands<'w, 's>,
}

fn update(mut params: UpdateCharactersParams) {
    let modified: Vec<_> = params
        .modified
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();
    for (entity, mut character, mut visibility) in &mut params.characters {
        let c = &mut *character;

        c.retrying &= !modified.contains(&c.model.id());
        let error = load_error(&params.server, c.model.id().untyped(), c.retrying).or_else(|| {
            c.motion
                .as_ref()
                .and_then(|motion| load_error(&params.server, motion.id().untyped(), false))
        });
        let loaded = !c.retrying
            && params.server.is_loaded_with_dependencies(c.model.id())
            && c.motion
                .as_ref()
                .is_none_or(|motion| params.server.is_loaded_with_dependencies(motion.id()));
        let next = if matches!(c.state, AvatarState::Error(_)) {
            c.state.clone()
        } else if let Some(error) = error {
            AvatarState::Error(error)
        } else if !loaded || c.root.is_none() {
            if loaded
                && !c.spawned
                && let Some(model) = params.models.get(&c.model)
                && let Some(scene) = model
                    .default_scene
                    .as_ref()
                    .or_else(|| model.scenes.first())
            {
                params.commands.spawn((
                    WorldAssetRoot(scene.clone()),
                    Transform::default(),
                    Visibility::Inherited,
                    ChildOf(entity),
                ));
                c.spawned = true;
            }
            AvatarState::Loading
        } else if params.outputs.is_empty() {
            AvatarState::Suspended
        } else {
            AvatarState::Ready
        };
        visibility.set_if_neq(if next == AvatarState::Ready {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        });
        if c.state != next {
            c.state = next;
            params.events.write(c.notification());
        }
        let running = c.config.playing && c.main.is_some() && c.state == AvatarState::Ready;
        let Some((mut player, spring)) = c.root.and_then(|root| params.players.get_mut(root).ok())
        else {
            continue;
        };
        if running {
            player.resume_all();
        } else {
            player.pause_all();
        }
        if c.pending.is_none()
            && let Some(animation) = c.main.and_then(|node| player.animation_mut(node))
        {
            if c.config.looping {
                animation.set_repeat(RepeatAnimation::Forever);
            } else if animation.repeat_mode() == RepeatAnimation::Forever {
                // Finish the current cycle, including after earlier loop completions.
                animation.set_repeat(RepeatAnimation::Count(
                    animation.completions().saturating_add(1),
                ));
            }
        }
        if let Some(mut spring) = spring {
            spring.paused = !running;
        }
    }
}

/// Syncs each character's direct gaze override onto its VRM root entity.
fn sync_gaze(characters: Query<&Character>, mut commands: Commands) {
    for c in &characters {
        let Some(root) = c.root else { continue };
        match c.gaze {
            Some((yaw_deg, pitch_deg)) if yaw_deg.is_finite() && pitch_deg.is_finite() => {
                commands.entity(root).try_insert(GazeOverride {
                    yaw_deg: yaw_deg.clamp(-90., 90.),
                    pitch_deg: pitch_deg.clamp(-60., 60.),
                });
            }
            _ => {
                commands.entity(root).try_remove::<GazeOverride>();
            }
        }
    }
}

/// Fades the mouth toward the amplitude the client sent, and the face toward the
/// expression it asked for, then hands both to ene-vrm as per-frame weights.
fn expressions(time: Res<Time>, mut characters: Query<&mut Character>, mut commands: Commands) {
    let dt = time.into_inner().delta_secs();
    for mut c in &mut characters {
        let Some(root) = c.root else { continue };
        // The mouth opens faster than it closes. That asymmetry is what makes
        // lip sync read as speech rather than as a tremolo.
        let tau = if c.lip > c.lip_level { 0.02 } else { 0.06 };
        c.lip_level += (c.lip - c.lip_level) * (1. - (-dt / tau).exp());
        if c.lip_level < 0.002 {
            c.lip_level = 0.;
        }
        let face = if c.expression.is_some() { 1. } else { 0. };
        if (face - c.expression_weight).abs() > 0.001 {
            let tau = if face > c.expression_weight {
                0.15
            } else {
                0.2
            };
            c.expression_weight += (face - c.expression_weight) * (1. - (-dt / tau).exp());
        } else {
            c.expression_weight = face;
        }
        let mut weights = Vec::with_capacity(2);
        if let Some(name) = &c.expression
            && c.expression_weight > 0.001
        {
            weights.push((name.clone(), c.expression_weight));
        }
        if c.lip_level > 0.001 {
            weights.push(("aa".to_owned(), c.lip_level));
        }
        // Missing while the scene loads; the next frame tries again.
        commands.entity(root).try_insert(ExpressionWeights(weights));
    }
}

#[cfg(test)]
mod tests {
    use super::motion::motion_node;
    use super::*;

    #[test]
    fn replay_during_a_fade_preserves_the_visible_mix_and_playback_positions() {
        let mut clips = Assets::<AnimationClip>::default();
        let first = clips.add(AnimationClip::default());
        let second = clips.add(AnimationClip::default());
        let (mut graph, a) = AnimationGraph::from_clip(first.clone());
        let b = graph.add_clip(second, 1., graph.root);
        let mut player = AnimationPlayer::default();
        let mut character = Character {
            state: AvatarState::Ready,
            ..default()
        };
        character.play_motion(&mut player, a);
        player.animation_mut(a).unwrap().set_seek_time(1.2);
        character.config.looping = false;
        character.play_motion(&mut player, b);
        player.animation_mut(b).unwrap().set_seek_time(0.1);
        character.advance_fades(&mut player, 0.1);
        let prior_a = *player.animation(a).unwrap();
        let prior_b = *player.animation(b).unwrap();
        assert!(prior_a.weight() > 0. && prior_b.weight() > 0.);

        let a_replay = motion_node(&mut graph, &player, &first);
        assert_ne!(a_replay, a);
        character.play_motion(&mut player, a_replay);
        character.advance_fades(&mut player, 0.);
        for (node, prior) in [(a, prior_a), (b, prior_b)] {
            let current = player.animation(node).unwrap();
            assert_eq!(current.seek_time(), prior.seek_time());
            assert_eq!(current.weight(), prior.weight());
            assert_eq!(current.repeat_mode(), prior.repeat_mode());
        }
        let incoming = player.animation(a_replay).unwrap();
        assert_eq!(incoming.seek_time(), 0.);
        assert!(incoming.weight() < 1e-6);

        character.advance_fades(&mut player, 0.21);
        assert!(player.animation(a).is_none());
        assert!(player.animation(b).unwrap().weight() > 0.);
        assert!(player.animation(a_replay).unwrap().weight() > 0.);
        character.advance_fades(&mut player, 0.1);
        assert_eq!(player.playing_animations().count(), 1);
        assert_eq!(player.animation(a_replay).unwrap().weight(), 1.);
        assert_eq!(motion_node(&mut graph, &player, &first), a);
    }
}
