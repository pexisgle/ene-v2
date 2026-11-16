use crate::desktop::Output;
use bevy::{
    animation::{RepeatAnimation, transition::AnimationTransitions},
    app::AnimationSystems,
    asset::{LoadState, RecursiveDependencyLoadState, UntypedAssetId},
    camera::visibility::VisibilitySystems,
    gltf::{Gltf, GltfAssetLabel},
    prelude::*,
    world_serialization::WorldInstanceReady,
};
use ene_vrm::{ExpressionWeights, SpringRig, Vrm, VrmSystems};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

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
    pub pending: Option<Pending>,
    /// The graph node of the clip that is the main animation for a fade.
    main: Option<AnimationNodeIndex>,
    /// The expression the client asked for, and how far the fade has got.
    pub expression: Option<String>,
    pub expression_weight: f32,
    /// The mouth opening the client asked for (from playback amplitude), and the
    /// smoothed value the blendshape actually uses.
    pub lip: f32,
    pub lip_level: f32,
    spawned: bool,
    retrying: bool,
}

/// A motion change that waits for its clip to load.
pub enum Pending {
    /// The client asked for this clip; it is loaded and faded in when ready.
    Fade(String),
    /// The client asked for no clip; the character returns to its rest pose.
    Rest,
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

#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub motion: Option<String>,
    pub playing: bool,
    pub looping: bool,
    pub monitor: Option<String>,
    /// Foot position in logical pixels from the monitor's top-left corner.
    pub position: Option<[f32; 2]>,
    /// 100 draws 1 m as 320 logical pixels.
    pub size_percent: f32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: String::new(),
            motion: None,
            playing: true,
            looping: true,
            monitor: None,
            position: None,
            size_percent: 100.,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        for path in std::iter::once(&self.model).chain(self.motion.iter()) {
            bevy::asset::AssetPath::try_parse(path).map_err(|error| error.to_string())?;
        }
        if !(50. ..=200.).contains(&self.size_percent)
            || self
                .position
                .is_some_and(|p| p.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.))
        {
            return Err("invalid size or position".into());
        }
        if !self.model.ends_with(".vrm")
            || self
                .motion
                .as_ref()
                .is_some_and(|path| !path.ends_with(".vrma"))
        {
            return Err("expected .vrm or .vrma path".into());
        }
        Ok(())
    }
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
        .add_observer(scene_ready)
        .add_systems(
            PostUpdate,
            update
                .before(AnimationSystems)
                .before(VisibilitySystems::VisibilityPropagate),
        )
        .add_systems(PostUpdate, notify_finished.after(AnimationSystems))
        // The mouth and the face are written after the animation reset of the
        // frame, and before ene-vrm turns weights into morph values.
        .add_systems(
            PostUpdate,
            (start_main, expressions, motion_change)
                .after(AnimationSystems)
                .before(VrmSystems::Expressions),
        )
        .configure_sets(
            PostUpdate,
            VrmSystems::SpringBone.before(VisibilitySystems::CalculateBounds),
        );
}

pub fn reload(world: &mut World, entity: Entity) {
    let character = world.get::<Character>(entity).unwrap();
    let server = world.resource::<AssetServer>();
    // Bevy denies paths outside its approved roots. The client selected these
    // files explicitly, so approve exactly these two loads and nothing else.
    let model = server
        .load_builder()
        .override_unapproved()
        .load(character.config.model.clone());
    let motion = character.config.motion.as_ref().map(|path| {
        server
            .load_builder()
            .override_unapproved()
            .load(GltfAssetLabel::Animation(0).from_asset(path.clone()))
    });
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
    let mut character = world.get_mut::<Character>(entity).unwrap();
    character.model = model;
    character.motion = motion;
    character.pending = None;
    character.root = None;
    character.spawned = false;
    character.retrying = retrying;
    character.state = AvatarState::Loading;
    let event = character.notification();
    world.write_message(event);
}

fn scene_ready(
    event: On<WorldInstanceReady>,
    parents: Query<&ChildOf>,
    children: Query<&Children>,
    mut characters: Query<&mut Character>,
    players: Query<&mut AnimationPlayer, With<Vrm>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut commands: Commands,
) {
    let Ok(parent) = parents.get(event.entity) else {
        return;
    };
    let Ok(mut character) = characters.get_mut(parent.parent()) else {
        return;
    };
    let Some(root) = children
        .iter_descendants(event.entity)
        .find(|e| players.contains(*e))
    else {
        return;
    };
    character.root = Some(root);
    if let Some(clip) = &character.motion {
        let (graph, node) = AnimationGraph::from_clip(clip.clone());
        commands.entity(root).insert((
            AnimationTransitions::new(),
            AnimationGraphHandle(graphs.add(graph)),
        ));
        character.main = Some(node);
    }
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

#[allow(clippy::too_many_arguments)]
fn update(
    mut characters: Query<(Entity, &mut Character, &mut Visibility)>,
    outputs: Query<&Output>,
    server: Res<AssetServer>,
    models: Res<Assets<Gltf>>,
    mut modified: MessageReader<AssetEvent<Gltf>>,
    mut players: Query<(&mut AnimationPlayer, Option<&mut SpringRig>)>,
    mut events: MessageWriter<Notification>,
    mut commands: Commands,
) {
    let modified: Vec<_> = modified
        .read()
        .filter_map(|event| match event {
            AssetEvent::Modified { id } => Some(*id),
            _ => None,
        })
        .collect();
    for (entity, mut character, mut visibility) in &mut characters {
        let c = &mut *character;

        c.retrying &= !modified.contains(&c.model.id());
        let error = load_error(&server, c.model.id().untyped(), c.retrying).or_else(|| {
            c.motion
                .as_ref()
                .and_then(|motion| load_error(&server, motion.id().untyped(), false))
        });
        let loaded = !c.retrying
            && server.is_loaded_with_dependencies(c.model.id())
            && c.motion
                .as_ref()
                .is_none_or(|motion| server.is_loaded_with_dependencies(motion.id()));
        let next = if matches!(c.state, AvatarState::Error(_)) {
            c.state.clone()
        } else if let Some(error) = error {
            AvatarState::Error(error)
        } else if !loaded || c.root.is_none() {
            if loaded
                && !c.spawned
                && let Some(model) = models.get(&c.model)
                && let Some(scene) = model
                    .default_scene
                    .as_ref()
                    .or_else(|| model.scenes.first())
            {
                commands.spawn((
                    WorldAssetRoot(scene.clone()),
                    Transform::default(),
                    Visibility::Inherited,
                    ChildOf(entity),
                ));
                c.spawned = true;
            }
            AvatarState::Loading
        } else if outputs.is_empty() {
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
            events.write(c.notification());
        }
        // No configured clip means the character rests: `idle` must not keep the
        // previous action running.
        let running =
            c.config.playing && c.config.motion.is_some() && c.state == AvatarState::Ready;
        let Some((mut player, spring)) = c.root.and_then(|root| players.get_mut(root).ok()) else {
            continue;
        };
        if running {
            player.resume_all();
        } else {
            player.pause_all();
        }
        for (_, animation) in player.playing_animations_mut() {
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

/// Starts the configured clip as the main animation of the transitions, so a
/// later motion change has something to fade out.
fn start_main(
    mut characters: Query<&mut Character>,
    mut roots: Query<(&mut AnimationPlayer, &mut AnimationTransitions)>,
) {
    for c in &mut characters {
        let Some(root) = c.root else { continue };
        let Some(node) = c.main else { continue };
        let Ok((mut player, mut transitions)) = roots.get_mut(root) else {
            continue;
        };
        if transitions.get_main_animation().is_some() {
            continue;
        }
        transitions.play(&mut player, node, Duration::ZERO).repeat();
    }
}
/// Fades the mouth toward the amplitude the client sent, and the face toward the
/// expression it asked for, then hands both to ene-vrm as per-frame weights.
fn expressions(time: Res<Time>, mut characters: Query<&mut Character>, mut commands: Commands) {
    let dt = time.delta_secs();
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

/// Applies a motion change once its clip is loaded, fading over 300 ms instead
/// of rebuilding the character.
fn motion_change(
    server: Res<AssetServer>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
    mut characters: Query<&mut Character>,
    // The player and its transitions live on the loaded model's root, which is a
    // child of the entity that carries `Character`.
    mut roots: Query<(
        &mut AnimationPlayer,
        Option<&AnimationGraphHandle>,
        Option<&mut AnimationTransitions>,
    )>,
) {
    for mut c in &mut characters {
        let Some(root) = c.root else { continue };
        let Some(pending) = c.pending.take() else {
            continue;
        };
        let Ok((mut player, Some(handle), Some(mut transitions))) = roots.get_mut(root) else {
            c.pending = Some(pending);
            continue;
        };
        match pending {
            Pending::Rest => {
                player.stop_all();
                c.motion = None;
            }
            Pending::Fade(path) => {
                let clip = server
                    .load_builder()
                    .override_unapproved()
                    .load(GltfAssetLabel::Animation(0).from_asset(path.clone()));
                if server
                    .get_load_states(clip.id())
                    .is_some_and(|(state, _, _)| matches!(state, LoadState::Failed(_)))
                {
                    warn!("the motion {path} did not load");
                    continue;
                }
                if !server.is_loaded_with_dependencies(clip.id()) {
                    c.pending = Some(Pending::Fade(path));
                    continue;
                }
                let Some(mut graph) = graphs.get_mut(&handle.0) else {
                    c.pending = Some(Pending::Fade(path));
                    continue;
                };
                let parent = graph.root;
                let node = graph.add_clip(clip.clone(), 1., parent);
                transitions.play(&mut player, node, Duration::from_millis(300));
                c.motion = Some(clip);
            }
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
            && let Some(player) = c.root.and_then(|root| players.get(root).ok())
            && player.all_finished()
            && player
                .playing_animations()
                .any(|(_, a)| !a.is_paused() && a.just_completed())
        {
            events.write(Notification(json!({"event":"finished", "character":c.id})));
        }
    }
}
