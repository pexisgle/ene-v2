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
use ene_vrm::{SpringRig, Vrm, VrmSystems};
use serde::{Deserialize, Serialize};
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
        .configure_sets(
            PostUpdate,
            VrmSystems::SpringBone.before(VisibilitySystems::CalculateBounds),
        );
}

pub fn reload(world: &mut World, entity: Entity) {
    let character = world.get::<Character>(entity).unwrap();
    let server = world.resource::<AssetServer>();
    let model = server.load(character.config.model.clone());
    let motion = character
        .config
        .motion
        .as_ref()
        .map(|path| server.load(GltfAssetLabel::Animation(0).from_asset(path.clone())));
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
    mut players: Query<&mut AnimationPlayer, With<Vrm>>,
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
        players.get_mut(root).unwrap().play(node).repeat();
        commands
            .entity(root)
            .insert(AnimationGraphHandle(graphs.add(graph)));
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
        let running = c.config.playing && c.state == AvatarState::Ready;
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
