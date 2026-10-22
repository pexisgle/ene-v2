use crate::desktop::Output;
use bevy::{
    app::AnimationSystems,
    asset::{LoadState, RecursiveDependencyLoadState},
    prelude::*,
};
use bevy::{
    gltf::{Gltf, GltfAssetLabel},
    world_serialization::WorldInstanceReady,
};
use ene_vrm::{SpringJointProps, Vrm, VrmSystems};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Lifecycle phase and load error, derived from the scene and available outputs.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "error", rename_all = "snake_case")]
pub enum AvatarState {
    #[default]
    Loading,
    Ready,
    Suspended,
    Error(String),
}

impl AvatarState {
    pub(crate) fn json(&self) -> Value {
        let mut value = json!(self);
        if !matches!(self, Self::Error(_)) {
            value["error"] = Value::Null;
        }
        value
    }
}

/// Identity and state root of one avatar. Presence of this component *is* the
/// definition of a character; there is no separate `Character` component.
#[derive(Component, Default)]
#[require(Transform, Visibility, AvatarState, Config)]
pub struct CharacterId(pub String);

/// Settings are accepted atomically by IPC and stored directly on the avatar.
#[derive(Component, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub model: String,
    pub motion: Option<String>,
    pub playing: bool,
    pub looping: bool,
    pub monitor: Option<String>,
    /// Foot position in logical pixels from the monitor's top-left corner.
    pub position: Option<[f32; 2]>,
    /// 100 draws 1 m as 160 logical pixels.
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

#[derive(Component)]
// The VRM solver has no individual pause; disabling bones would also stop transform updates.
pub(crate) struct PausedSpring(SpringJointProps);

#[derive(Component)]
struct SpringDisplayScale(f32);

#[derive(Message)]
pub struct Notification(pub Value);

pub fn setup(app: &mut App) {
    app.add_message::<Notification>()
        .add_observer(scene_ready)
        .add_systems(PostUpdate, load_scene.before(update_avatars))
        .add_systems(
            PostUpdate,
            update_avatars
                .before(AnimationSystems)
                .before(bevy::camera::visibility::VisibilitySystems::VisibilityPropagate),
        )
        .add_systems(PostUpdate, notify_finished.after(AnimationSystems))
        .configure_sets(
            PostUpdate,
            VrmSystems::SpringBone
                .before(bevy::camera::visibility::VisibilitySystems::CalculateBounds),
        );
}

/// The single place that decides an avatar's state and applies the transition.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn update_avatars(
    mut commands: Commands,
    mut actors: Query<(
        Entity,
        &CharacterId,
        &Config,
        &Transform,
        &mut AvatarState,
        &mut Visibility,
        Has<SceneReady>,
        Option<&ModelAssets>,
    )>,
    ready_scenes: Query<(), With<SceneReady>>,
    outputs: Query<&Output>,
    server: Res<AssetServer>,
    children: Query<&Children>,
    mut parts: Query<(
        Entity,
        AnyOf<(
            &mut AnimationPlayer,
            &mut SpringJointProps,
            &mut PausedSpring,
        )>,
        Option<&SpringDisplayScale>,
    )>,
    mut states: MessageWriter<Notification>,
) {
    use bevy::animation::RepeatAnimation;
    for (root, id, config, transform, mut state, mut visibility, initialized, assets) in &mut actors
    {
        let initialized = assets.map_or(initialized, |a| ready_scenes.contains(a.scene));
        let motion_ready = config.motion.is_none()
            || parts
                .iter_many(std::iter::once(root).chain(children.iter_descendants(root)))
                .any(|(_, (player, _, _), _)| {
                    player.is_some_and(|player| player.playing_animations().next().is_some())
                });
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
        while let Some((entity, (player, mut props, mut paused), previous_scale)) =
            parts.fetch_next()
        {
            let previous_scale = previous_scale.map_or(1., |scale| scale.0);
            if transform.scale.x != previous_scale
                && let Some(props) = props
                    .as_deref_mut()
                    .or_else(|| paused.as_deref_mut().map(|paused| &mut paused.0))
            {
                // Preserve the same motion when model meters are rendered as desktop pixels.
                let ratio = transform.scale.x / previous_scale;
                props.gravity_power *= ratio;
                props.stiffness *= ratio;
                props.hit_radius *= ratio;
                commands
                    .entity(entity)
                    .insert(SpringDisplayScale(transform.scale.x));
            }
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
            if running {
                if let Some(paused) = paused {
                    commands
                        .entity(entity)
                        .insert(paused.0)
                        .remove::<PausedSpring>();
                }
            } else if let Some(props) = props {
                commands
                    .entity(entity)
                    .insert(PausedSpring(*props))
                    .remove::<SpringJointProps>();
            }
        }
    }
}

pub(crate) fn reload(
    In(entity): In<Entity>,
    mut commands: Commands,
    actors: Query<(&CharacterId, &Config, Option<&ModelAssets>)>,
    server: Res<AssetServer>,
    mut states: MessageWriter<Notification>,
) {
    let Ok((id, config, _)) = actors.get(entity) else {
        return;
    };
    if let Ok((_, _, Some(old))) = actors.get(entity) {
        commands.entity(old.scene).despawn();
    }
    commands
        .entity(entity)
        .insert((AvatarState::Loading, Visibility::Hidden));
    let scene = commands
        .spawn((Transform::default(), Visibility::Inherited, ChildOf(entity)))
        .id();
    for path in std::iter::once(&config.model).chain(config.motion.iter()) {
        if server
            .get_path_id(path.clone())
            .is_some_and(|id| matches!(server.get_load_state(id), Some(LoadState::Failed(_))))
        {
            server.reload(path.clone());
        }
    }
    commands.entity(entity).insert(ModelAssets {
        model: server.load(config.model.clone()),
        motion: config
            .motion
            .as_ref()
            .map(|path| server.load(GltfAssetLabel::Animation(0).from_asset(path.clone()))),
        scene,
    });
    states.write(Notification(
        json!({"event":"state", "character":id.0, "status":"loading", "error":null}),
    ));
}

#[derive(Component)]
pub(crate) struct ModelAssets {
    model: Handle<Gltf>,
    motion: Option<Handle<AnimationClip>>,
    scene: Entity,
}

#[derive(Component)]
pub(crate) struct SceneReady;

fn scene_ready(
    event: On<WorldInstanceReady>,
    mut commands: Commands,
    actors: Query<&ModelAssets>,
    parents: Query<&ChildOf>,
) {
    let target = event.entity;
    if let Ok(parent) = parents.get(target)
        && let Ok(assets) = actors.get(parent.parent())
        && assets.scene == target
    {
        commands.entity(target).insert(SceneReady);
    }
}

#[allow(clippy::too_many_arguments)]
fn load_scene(
    mut commands: Commands,
    actors: Query<&ModelAssets>,
    gltfs: Res<Assets<Gltf>>,
    worlds: Res<Assets<WorldAsset>>,
    server: Res<AssetServer>,
    scenes: Query<(Has<WorldAssetRoot>, Has<SceneReady>)>,
    children: Query<&Children>,
    rigs: Query<(), With<Vrm>>,
    graphs_present: Query<(), With<AnimationGraphHandle>>,
    mut graphs: ResMut<Assets<AnimationGraph>>,
) {
    for assets in &actors {
        let Some(model) = gltfs.get(&assets.model) else {
            continue;
        };
        let Ok((spawned, ready)) = scenes.get(assets.scene) else {
            continue;
        };
        if !spawned {
            if let Some(scene) = model
                .default_scene
                .as_ref()
                .or_else(|| model.scenes.first())
                && worlds.contains(scene.id())
                && server.is_loaded_with_dependencies(scene.id())
            {
                commands
                    .entity(assets.scene)
                    .insert(WorldAssetRoot(scene.clone()));
            }
        } else if ready
            && let Some(clip) = &assets.motion
            && server.is_loaded_with_dependencies(clip.id())
        {
            for root in children
                .iter_descendants(assets.scene)
                .filter(|e| rigs.contains(*e) && !graphs_present.contains(*e))
            {
                let (graph, node) = AnimationGraph::from_clip(clip.clone());
                let mut player = AnimationPlayer::default();
                player.play(node).repeat();
                commands.entity(root).insert((
                    player,
                    AnimationGraphHandle(graphs.add(graph)),
                    AnimationTransitions::new(),
                ));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{Drag, HitRegions, bounds, desktop_position};
    use bevy::ecs::message::Messages;
    use bevy::ecs::system::RunSystemOnce;

    fn scene_app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .add_message::<Notification>();
        app.world_mut().spawn(Output {
            name: "m".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        });
        app
    }

    #[test]
    fn spring_display_unit_conversion_survives_pause_and_size_changes() {
        let mut app = scene_app();
        app.add_systems(Update, update_avatars);
        let root = app
            .world_mut()
            .spawn((
                CharacterId("scale".into()),
                SceneReady,
                AvatarState::Ready,
                Transform::from_scale(Vec3::splat(160.)),
            ))
            .id();
        let original = SpringJointProps {
            gravity_power: 1.,
            stiffness: 0.5,
            hit_radius: 0.02,
            ..default()
        };
        let joint = app.world_mut().spawn((original, ChildOf(root))).id();
        app.update();
        let scaled = *app.world().get::<SpringJointProps>(joint).unwrap();
        assert_eq!(scaled.gravity_power, 160.);
        assert_eq!(scaled.stiffness, 80.);
        assert_eq!(scaled.hit_radius, original.hit_radius * 160.);
        app.world_mut().get_mut::<Config>(root).unwrap().playing = false;
        app.update();
        assert!(app.world().get::<SpringJointProps>(joint).is_none());
        app.world_mut().get_mut::<Transform>(root).unwrap().scale = Vec3::splat(320.);
        app.update();
        let paused = app.world().get::<PausedSpring>(joint).unwrap().0;
        assert_eq!(paused.gravity_power, 320.);
        assert_eq!(paused.stiffness, 160.);
        assert_eq!(paused.hit_radius, original.hit_radius * 320.);
        app.world_mut().get_mut::<Config>(root).unwrap().playing = true;
        app.update();
        app.update();
        assert_eq!(*app.world().get::<SpringJointProps>(joint).unwrap(), paused);
    }

    #[test]
    fn native_scene_loading_errors_retry_and_replacement_reach_a_terminal_state() {
        let directory =
            std::env::temp_dir().join(format!("ene-avatar-native-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let model = json!({"asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],"nodes":[{"translation":[0,1,0]}],"extensions":{"VRMC_vrm":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":0}}}}}});
        let good = directory.join("good.vrm");
        let bad = directory.join("bad.vrm");
        std::fs::write(&good, model.to_string()).unwrap();
        std::fs::write(&bad, "{}").unwrap();
        let motion = directory.join("motion.vrma");
        let bad_motion = directory.join("bad.vrma");
        let values: Vec<f32> = vec![0., 1., 0., 1., 0., 0., 1., 0.];
        std::fs::write(
            directory.join("motion.bin"),
            values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut motion_value = model.clone();
        motion_value["extensions"] = json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":0}}}}});
        motion_value["buffers"] = json!([{"byteLength":32,"uri":"motion.bin"}]);
        motion_value["bufferViews"] =
            json!([{"buffer":0,"byteLength":8},{"buffer":0,"byteOffset":8,"byteLength":24}]);
        motion_value["accessors"] = json!([{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},{"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}]);
        motion_value["animations"] = json!([{"samplers":[{"input":0,"output":1}],"channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]}]);
        std::fs::write(&motion, motion_value.to_string()).unwrap();
        std::fs::write(&bad_motion, "{}").unwrap();
        let plugins = DefaultPlugins
            .build()
            .disable::<AssetPlugin>()
            .disable::<bevy::render::RenderPlugin>()
            .disable::<bevy::log::LogPlugin>();
        #[cfg(windows)]
        let plugins = plugins.disable::<bevy::winit::WinitPlugin>();
        let mut app = App::new();
        app.add_plugins(AssetPlugin {
            unapproved_path_mode: bevy::asset::UnapprovedPathMode::Allow,
            ..default()
        })
        .init_asset::<Shader>()
        .init_asset_loader::<bevy::shader::ShaderLoader>()
        .add_plugins(plugins.set(WindowPlugin {
            primary_window: None,
            ..default()
        }))
        .add_plugins((
            bevy::render::sync_world::SyncWorldPlugin,
            ene_vrm::VrmPlugin,
            setup,
        ));
        app.world_mut().spawn(Output {
            name: "test".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        });
        let actors: Vec<_> = [
            (&good, Some(&motion)),
            (&bad, None),
            (&good, Some(&motion)),
            (&good, Some(&bad_motion)),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, (path, motion))| {
            app.world_mut()
                .spawn((
                    CharacterId(i.to_string()),
                    Config {
                        model: path.to_str().unwrap().into(),
                        motion: motion.map(|p| p.to_str().unwrap().into()),
                        ..default()
                    },
                ))
                .id()
        })
        .collect();
        for actor in &actors {
            app.world_mut()
                .run_system_cached_with(reload, *actor)
                .unwrap();
        }
        let replaced = app.world().get::<ModelAssets>(actors[2]).unwrap().scene;
        app.world_mut()
            .run_system_cached_with(reload, actors[2])
            .unwrap();
        assert!(app.world().get_entity(replaced).is_err());
        app.finish();
        app.cleanup();
        for repair in [false, true] {
            if repair {
                std::fs::write(&bad, model.to_string()).unwrap();
                std::fs::write(&bad_motion, motion_value.to_string()).unwrap();
                for i in [1, 3] {
                    app.world_mut()
                        .run_system_cached_with(reload, actors[i])
                        .unwrap();
                }
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                app.update();
                if actors.iter().enumerate().all(|(i, e)| {
                    match app.world().get::<AvatarState>(*e).unwrap() {
                        AvatarState::Ready => ![1, 3].contains(&i) || repair,
                        AvatarState::Error(_) => [1, 3].contains(&i) && !repair,
                        _ => false,
                    }
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "native lifecycle stalled"
                );
                std::thread::yield_now();
            }
        }
        assert_eq!(app.world_mut().query::<&Vrm>().iter(app.world()).count(), 4);
        for actor in actors {
            app.world_mut().entity_mut(actor).despawn();
        }
        app.update();
        assert_eq!(app.world_mut().query::<&Vrm>().iter(app.world()).count(), 0);
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn pause_and_resume_only_touch_the_selected_characters_players_and_springs() {
        let mut app = scene_app();
        let world = app.world_mut();
        let mut actors = Vec::new();
        for playing in [false, true] {
            let root = world.spawn_empty().id();
            let mut players = Vec::new();
            for _ in 0..2 {
                let mut player = AnimationPlayer::default();
                player
                    .play(AnimationNodeIndex::new(0))
                    .repeat()
                    .seek_to(1.25);
                players.push(world.spawn((player, ChildOf(root))).id());
            }
            let spring = world
                .spawn((SpringJointProps::default(), ChildOf(root)))
                .id();
            world.entity_mut(root).insert((
                CharacterId(root.to_bits().to_string()),
                AvatarState::Ready,
                SceneReady,
                Config {
                    playing,
                    looping: true,
                    ..default()
                },
            ));
            actors.push((root, players, spring));
        }

        world.run_system_once(update_avatars).unwrap();
        for (index, (_, players, spring)) in actors.iter().enumerate() {
            for &player in players {
                let player = world.get::<AnimationPlayer>(player).unwrap();
                assert_eq!(player.all_paused(), index == 0);
                assert_eq!(
                    player.playing_animations().next().unwrap().1.seek_time(),
                    1.25
                );
            }
            assert_eq!(world.get::<SpringJointProps>(*spring).is_none(), index == 0);
            assert_eq!(world.get::<PausedSpring>(*spring).is_some(), index == 0);
        }
        world.get_mut::<Config>(actors[0].0).unwrap().playing = true;
        world.run_system_once(update_avatars).unwrap();
        assert!(
            !world
                .get::<AnimationPlayer>(actors[0].1[0])
                .unwrap()
                .all_paused()
        );
        assert!(world.get::<SpringJointProps>(actors[0].2).is_some());
        assert!(world.get::<PausedSpring>(actors[0].2).is_none());
    }

    #[test]
    fn unchanged_controls_do_not_rewrite_players_and_late_parts_inherit_pause() {
        let mut app = scene_app();
        app.add_systems(Update, update_avatars);
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                AvatarState::Ready,
                SceneReady,
                Config {
                    playing: false,
                    looping: true,
                    ..default()
                },
            ))
            .id();
        let mut player = AnimationPlayer::default();
        player
            .play(AnimationNodeIndex::new(0))
            .repeat()
            .seek_to(1.25);
        let player = app.world_mut().spawn((player, ChildOf(avatar))).id();
        app.update();
        let changed = app
            .world()
            .entity(player)
            .get_ref::<AnimationPlayer>()
            .unwrap()
            .last_changed();
        app.world_mut().get_mut::<Config>(avatar).unwrap().position = Some([20., 30.]);
        app.update();
        assert_eq!(
            app.world()
                .entity(player)
                .get_ref::<AnimationPlayer>()
                .unwrap()
                .last_changed(),
            changed
        );

        let mut late = AnimationPlayer::default();
        late.play(AnimationNodeIndex::new(0));
        let late = app.world_mut().spawn((late, ChildOf(avatar))).id();
        let spring = app
            .world_mut()
            .spawn((SpringJointProps::default(), ChildOf(avatar)))
            .id();
        app.update();
        assert!(
            app.world()
                .get::<AnimationPlayer>(late)
                .unwrap()
                .all_paused()
        );
        assert!(app.world().get::<PausedSpring>(spring).is_some());
        assert!(app.world().get::<SpringJointProps>(spring).is_none());
        let other = app
            .world_mut()
            .spawn((CharacterId("b".into()), AvatarState::Ready, SceneReady))
            .id();
        app.world_mut().entity_mut(late).insert(ChildOf(other));
        app.world_mut().entity_mut(spring).insert(ChildOf(other));
        app.update();
        assert!(
            !app.world()
                .get::<AnimationPlayer>(late)
                .unwrap()
                .all_paused()
        );
        assert!(app.world().get::<SpringJointProps>(spring).is_some());
        assert!(app.world().get::<PausedSpring>(spring).is_none());
    }

    #[test]
    fn completion_uses_the_current_tick_and_rearms_after_looping() {
        fn step(app: &mut App, frames: usize) -> Vec<String> {
            let mut events = Vec::new();
            for _ in 0..frames {
                app.update();
                events.extend(
                    app.world_mut()
                        .resource_mut::<Messages<Notification>>()
                        .drain()
                        .filter(|event| event.0["event"] == "finished")
                        .map(|event| event.0["character"].as_str().unwrap().to_owned()),
                );
            }
            events
        }
        let mut app = scene_app();
        app.init_resource::<Assets<AnimationClip>>()
            .init_resource::<Assets<AnimationGraph>>()
            .add_message::<Notification>()
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_millis(250),
            ))
            .add_systems(
                PostUpdate,
                (
                    update_avatars,
                    bevy::animation::advance_animations,
                    notify_finished,
                )
                    .chain(),
            );
        let mut clip = AnimationClip::default();
        clip.set_duration(1.);
        let clip = app
            .world_mut()
            .resource_mut::<Assets<AnimationClip>>()
            .add(clip);
        let (graph, node) = AnimationGraph::from_clip(clip);
        let graph = app
            .world_mut()
            .resource_mut::<Assets<AnimationGraph>>()
            .add(graph);
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                AvatarState::Ready,
                SceneReady,
                Config {
                    playing: true,
                    looping: false,
                    motion: Some("test.vrma".into()),
                    ..default()
                },
            ))
            .id();
        let other = app
            .world_mut()
            .spawn((CharacterId("b".into()), AvatarState::Ready, SceneReady))
            .id();
        for (index, root) in [avatar, avatar, other].into_iter().enumerate() {
            let mut player = AnimationPlayer::default();
            player
                .play(node)
                .set_speed(if index == 1 { 0.5 } else { 1. });
            app.world_mut()
                .spawn((player, AnimationGraphHandle(graph.clone()), ChildOf(root)));
        }
        app.world_mut()
            .spawn((AnimationPlayer::default(), ChildOf(avatar)));
        assert!(step(&mut app, 1).is_empty());
        assert!(step(&mut app, 4).is_empty());
        assert_eq!(step(&mut app, 4), ["a"]);
        assert!(step(&mut app, 4).is_empty());
        app.world_mut().get_mut::<Config>(avatar).unwrap().looping = true;
        assert!(step(&mut app, 8).is_empty());
        app.world_mut().get_mut::<Config>(avatar).unwrap().looping = false;
        assert_eq!(step(&mut app, 8), ["a"]);
        assert!(step(&mut app, 4).is_empty());
    }

    #[test]
    fn state_transitions_follow_loading_ready_suspended_and_error() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .add_message::<Notification>()
            .add_systems(Update, update_avatars);
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                Config {
                    model: "a.vrm".into(),
                    ..default()
                },
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Loading)
        );

        let output = app
            .world_mut()
            .spawn(Output {
                name: "m".into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            })
            .id();
        app.update();
        assert_eq!(
            app.world().get::<Visibility>(avatar),
            Some(&Visibility::Hidden)
        );

        app.world_mut().entity_mut(avatar).insert(SceneReady);
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Ready)
        );
        assert_eq!(
            app.world().get::<Visibility>(avatar),
            Some(&Visibility::Inherited)
        );

        app.world_mut().entity_mut(output).despawn();
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Suspended)
        );

        app.world_mut()
            .entity_mut(avatar)
            .insert(AvatarState::Error("broken".into()));
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Error("broken".into()))
        );
        assert_eq!(
            app.world().get::<Visibility>(avatar),
            Some(&Visibility::Hidden)
        );
        app.world_mut().spawn(Output {
            name: "m".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        });
        app.update();
        assert!(matches!(
            app.world().get::<AvatarState>(avatar),
            Some(AvatarState::Error(_))
        ));
        app.world_mut().entity_mut(avatar).insert((
            AvatarState::Loading,
            ModelAssets {
                model: Handle::default(),
                motion: None,
                scene: Entity::PLACEHOLDER,
            },
        ));
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Loading)
        );
        app.world_mut().entity_mut(avatar).remove::<ModelAssets>();
        app.world_mut().get_mut::<Config>(avatar).unwrap().motion = Some("m.vrma".into());
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Loading)
        );
        app.world_mut().get_mut::<Config>(avatar).unwrap().playing = false;
        let mut player = AnimationPlayer::default();
        player.play(AnimationNodeIndex::new(0));
        let player = app.world_mut().spawn((player, ChildOf(avatar))).id();
        app.update();
        assert_eq!(
            app.world().get::<AvatarState>(avatar),
            Some(&AvatarState::Ready)
        );
        let animation = app
            .world()
            .get::<AnimationPlayer>(player)
            .unwrap()
            .animation(AnimationNodeIndex::new(0))
            .unwrap();
        assert!(animation.is_paused());
        assert_eq!(
            animation.repeat_mode(),
            bevy::animation::RepeatAnimation::Forever
        );
    }

    #[test]
    fn asset_server_failure_becomes_an_error_notification() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<Image>()
            .register_asset_loader(bevy::image::ImageLoader::new(
                bevy::image::CompressedImageFormats::NONE,
            ))
            .add_message::<Notification>()
            .add_systems(Update, update_avatars);
        let path = "__ene_avatar_missing_asset_for_state_test__.png";
        let _asset = app.world().resource::<AssetServer>().load::<Image>(path);
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                Config {
                    model: path.into(),
                    ..default()
                },
            ))
            .id();
        for _ in 0..10_000 {
            app.update();
            if matches!(
                app.world().get::<AvatarState>(avatar),
                Some(AvatarState::Error(_))
            ) {
                assert_eq!(
                    app.world().get::<Visibility>(avatar),
                    Some(&Visibility::Hidden)
                );
                assert!(
                    app.world_mut()
                        .resource_mut::<Messages<Notification>>()
                        .drain()
                        .any(|event| event.0["status"] == "error" && event.0["error"].is_string())
                );
                return;
            }
            std::thread::yield_now();
        }
        panic!("the asset failure was not reported");
    }

    #[test]
    fn desktop_coordinates_use_monitor_origin_and_dpi() {
        let output = Output {
            name: "left".into(),
            origin: Vec2::new(-3840., 0.),
            size: Vec2::new(3840., 2160.),
            units_per_logical_pixel: 2.,
        };
        assert_eq!(
            desktop_position(&output, Vec2::new(100., 200.)),
            Vec2::new(-3640., 400.)
        );
        assert!(bounds(&output).contains(Vec2::new(-1., 100.)));
    }

    #[test]
    fn pointer_drag_across_dpi_updates_placement_and_destination() {
        #[derive(Resource, Default)]
        struct Clicks(u32);
        fn count_clicks(mut clicks: ResMut<Clicks>, mut messages: MessageReader<Notification>) {
            clicks.0 += messages
                .read()
                .filter(|event| event.0["event"] == "click")
                .count() as u32;
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<crate::desktop::PointerInput>()
            .add_message::<Notification>()
            .init_resource::<Clicks>()
            .init_resource::<Drag>()
            .add_systems(Update, (crate::desktop::pointer, count_clicks).chain());
        let actor = app
            .world_mut()
            .spawn((
                CharacterId("a".into()),
                AvatarState::Ready,
                SceneReady,
                Config {
                    monitor: Some("left".into()),
                    position: Some([50., 50.]),
                    ..default()
                },
                Transform::from_xyz(50., -50., 0.),
            ))
            .id();
        let left = app
            .world_mut()
            .spawn((
                Output {
                    name: "left".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                },
                HitRegions(vec![(
                    actor,
                    Rect::from_corners(Vec2::ZERO, Vec2::splat(100.)),
                )]),
            ))
            .id();
        let right = app
            .world_mut()
            .spawn(Output {
                name: "right".into(),
                origin: Vec2::new(100., 0.),
                size: Vec2::splat(200.),
                units_per_logical_pixel: 2.,
            })
            .id();
        app.world_mut().write_message(crate::desktop::PointerInput {
            output: left,
            local: Vec2::new(10., 20.),
            button: Some(true),
        });
        app.update();
        app.world_mut().write_message(crate::desktop::PointerInput {
            output: right,
            local: Vec2::new(5., 10.),
            button: None,
        });
        app.update();

        let placement = app.world().get::<Config>(actor).unwrap();
        assert_eq!(placement.monitor.as_deref(), Some("right"));
        assert_eq!(placement.position, Some([45., 40.]));
        app.world_mut().write_message(crate::desktop::PointerInput {
            output: left,
            local: Vec2::new(80., 20.),
            button: None,
        });
        app.update();
        let placement = app.world().get::<Config>(actor).unwrap();
        assert_eq!(placement.monitor.as_deref(), Some("left"));
        assert_eq!(placement.position, Some([120., 50.]));

        app.world_mut().write_message(crate::desktop::PointerInput {
            output: right,
            local: Vec2::new(5., 10.),
            button: Some(false),
        });
        app.update();
        app.world_mut().write_message(crate::desktop::PointerInput {
            output: left,
            local: Vec2::new(10., 20.),
            button: Some(true),
        });
        app.update();
        app.world_mut().write_message(crate::desktop::PointerInput {
            output: left,
            local: Vec2::new(10., 20.),
            button: Some(false),
        });
        app.update();
        assert_eq!(app.world().resource::<Clicks>().0, 1);
    }
}
