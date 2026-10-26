use super::lifecycle::{notify_finished, update_avatars};
use super::*;
use crate::desktop::Output;
use bevy::ecs::message::Messages;
use bevy::gltf::Gltf;
use bevy::prelude::*;
use ene_vrm::{SpringRig, Vrm};
use serde_json::{Value, json};
use std::time::Duration;

fn scene_app() -> (App, Entity) {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Gltf>()
        .add_message::<Notification>()
        .add_systems(Update, update_avatars);
    let output = spawn_output(&mut app);
    (app, output)
}

fn drain_events(app: &mut App) -> Vec<Value> {
    app.world_mut()
        .resource_mut::<Messages<Notification>>()
        .drain()
        .map(|notification| notification.0)
        .collect()
}

fn spawn_output(app: &mut App) -> Entity {
    app.world_mut()
        .spawn(Output {
            name: "m".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        })
        .id()
}

fn loaded_model(app: &mut App) -> Handle<Gltf> {
    app.world().resource::<AssetServer>().add(Gltf {
        scenes: Vec::new(),
        named_scenes: Default::default(),
        meshes: Vec::new(),
        named_meshes: Default::default(),
        materials: Vec::new(),
        named_materials: Default::default(),
        nodes: Vec::new(),
        named_nodes: Default::default(),
        skins: Vec::new(),
        named_skins: Default::default(),
        default_scene: None,
        animations: Vec::new(),
        named_animations: Default::default(),
        source: None,
    })
}

/// Simulates the scene instance `load_scene` binds after a VRM spawns.
fn bind(app: &mut App, avatar: Entity) -> Entity {
    let model = loaded_model(app);
    let root = app
        .world_mut()
        .spawn((Vrm::default(), AnimationPlayer::default(), ChildOf(avatar)))
        .id();
    app.world_mut()
        .entity_mut(avatar)
        .insert(ModelAssets::with_model(root, model));
    root
}

#[test]
fn state_transitions() {
    let (mut app, output) = scene_app();
    let avatar = app
        .world_mut()
        .spawn((
            CharacterId("a".into()),
            Config {
                model: "a.vrm".into(),
                motion: Some("a.vrma".into()),
                ..default()
            },
        ))
        .id();
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Loading)
    );
    assert_eq!(
        app.world().get::<Visibility>(avatar),
        Some(&Visibility::Hidden)
    );
    assert!(drain_events(&mut app).is_empty());

    let root = bind(&mut app, avatar);
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Loading)
    );
    assert!(drain_events(&mut app).is_empty());

    let mut player = AnimationPlayer::default();
    player.play(AnimationNodeIndex::new(0));
    app.world_mut().entity_mut(root).insert(player);
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Ready)
    );
    assert_eq!(
        app.world().get::<Visibility>(avatar),
        Some(&Visibility::Inherited)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"ready","error":null})]
    );

    app.world_mut()
        .get_mut::<ModelAssets>(avatar)
        .unwrap()
        .retrying_model_dependencies = true;
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Loading)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"loading","error":null})]
    );
    app.world_mut()
        .get_mut::<ModelAssets>(avatar)
        .unwrap()
        .retrying_model_dependencies = false;
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Ready)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"ready","error":null})]
    );

    app.world_mut().despawn(output);
    app.update();
    assert_eq!(
        app.world().get::<AvatarState>(avatar),
        Some(&AvatarState::Suspended)
    );
    assert_eq!(
        drain_events(&mut app),
        [json!({"event":"state","character":"a","status":"suspended","error":null})]
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
    assert!(drain_events(&mut app).is_empty());
    app.update();
    assert!(drain_events(&mut app).is_empty());

    spawn_output(&mut app);
    app.update();
    assert!(matches!(
        app.world().get::<AvatarState>(avatar),
        Some(AvatarState::Error(_))
    ));
    assert!(drain_events(&mut app).is_empty());
}

#[test]
fn pause_and_resume() {
    let (mut app, _) = scene_app();
    let model = loaded_model(&mut app);
    let mut actors = Vec::new();
    for playing in [false, true] {
        let avatar = app
            .world_mut()
            .spawn((
                CharacterId(playing.to_string()),
                AvatarState::Ready,
                Config {
                    playing,
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
        let root = app
            .world_mut()
            .spawn((
                Vrm::default(),
                player,
                SpringRig::default(),
                ChildOf(avatar),
            ))
            .id();
        app.world_mut()
            .entity_mut(avatar)
            .insert(ModelAssets::with_model(root, model.clone()));
        actors.push((avatar, root));
    }

    app.update();
    for (index, (_, root)) in actors.iter().enumerate() {
        let player = app.world().get::<AnimationPlayer>(*root).unwrap();
        assert_eq!(player.all_paused(), index == 0);
        assert_eq!(
            player.playing_animations().next().unwrap().1.seek_time(),
            1.25
        );
        assert_eq!(
            app.world().get::<SpringRig>(*root).unwrap().paused,
            index == 0
        );
    }

    let (avatar, root) = actors[0];
    app.world_mut().get_mut::<Config>(avatar).unwrap().playing = true;
    app.update();
    assert!(
        !app.world()
            .get::<AnimationPlayer>(root)
            .unwrap()
            .all_paused()
    );
    assert!(!app.world().get::<SpringRig>(root).unwrap().paused);

    app.world_mut().get_mut::<Config>(avatar).unwrap().playing = false;
    app.update();
    assert!(
        app.world()
            .get::<AnimationPlayer>(root)
            .unwrap()
            .all_paused()
    );
    assert!(app.world().get::<SpringRig>(root).unwrap().paused);
}

#[test]
fn finished_and_looping() {
    fn step(app: &mut App, frames: usize) -> Vec<String> {
        let mut events = Vec::new();
        for _ in 0..frames {
            app.update();
            events.extend(
                drain_events(app)
                    .into_iter()
                    .filter(|event| event["event"] == "finished")
                    .map(|event| event["character"].as_str().unwrap().to_owned()),
            );
        }
        events
    }

    let (mut app, _) = scene_app();
    let model = loaded_model(&mut app);
    app.init_resource::<Assets<AnimationClip>>()
        .init_resource::<Assets<AnimationGraph>>()
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(250),
        ))
        .add_systems(
            PostUpdate,
            (bevy::animation::advance_animations, notify_finished).chain(),
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
        .spawn((
            CharacterId("b".into()),
            AvatarState::Ready,
            Config {
                motion: Some("test.vrma".into()),
                ..default()
            },
        ))
        .id();
    for root in [avatar, other] {
        let mut player = AnimationPlayer::default();
        player.play(node);
        let instance = app
            .world_mut()
            .spawn((
                Vrm::default(),
                player,
                AnimationGraphHandle(graph.clone()),
                ChildOf(root),
            ))
            .id();
        app.world_mut()
            .entity_mut(root)
            .insert(ModelAssets::with_model(instance, model.clone()));
    }

    assert!(step(&mut app, 1).is_empty());
    assert_eq!(step(&mut app, 4), ["a"]);
    assert!(step(&mut app, 4).is_empty());
    app.world_mut().get_mut::<Config>(avatar).unwrap().looping = true;
    assert!(step(&mut app, 8).is_empty());
    app.world_mut().get_mut::<Config>(avatar).unwrap().looping = false;
    assert_eq!(step(&mut app, 8), ["a"]);
    assert!(step(&mut app, 4).is_empty());
}
