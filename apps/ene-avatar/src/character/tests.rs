use super::lifecycle::update_avatars;
use super::*;
use crate::desktop::Output;
use bevy::ecs::message::Messages;
use bevy::ecs::system::RunSystemOnce;
use ene_vrm::{HumanBoneName, SpringJointProps, SpringRig, Vrm};
use serde_json::json;

fn ready() -> Vrm {
    Vrm::default()
}

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
fn spring_spec_values_survive_pause_and_scale_changes_without_app_scaling() {
    let mut app = scene_app();
    app.add_systems(Update, update_avatars);
    let root = app
        .world_mut()
        .spawn((
            CharacterId("scale".into()),
            ready(),
            AvatarState::Ready,
            SpringRig::default(),
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
    assert_eq!(
        *app.world().get::<SpringJointProps>(joint).unwrap(),
        original
    );

    app.world_mut().get_mut::<Config>(root).unwrap().playing = false;
    app.update();
    assert_eq!(
        *app.world().get::<SpringJointProps>(joint).unwrap(),
        original
    );
    assert!(app.world().get::<SpringRig>(root).unwrap().paused);

    app.world_mut().get_mut::<Transform>(root).unwrap().scale = Vec3::splat(320.);
    app.update();
    assert_eq!(
        *app.world().get::<SpringJointProps>(joint).unwrap(),
        original
    );
    assert!(app.world().get::<SpringRig>(root).unwrap().paused);

    app.world_mut().get_mut::<Config>(root).unwrap().playing = true;
    app.update();
    app.update();
    assert_eq!(
        *app.world().get::<SpringJointProps>(joint).unwrap(),
        original
    );
    assert!(!app.world().get::<SpringRig>(root).unwrap().paused);
}

fn native_app() -> App {
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
    ))
    .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1. / 60.),
    ));
    app.finish();
    app.cleanup();
    app
}

#[test]
fn native_scene_loading_errors_retry_and_replacement_reach_a_terminal_state() {
    let directory = std::env::temp_dir().join(format!("ene-avatar-native-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let nodes = vec![
        json!({"children":[1,3,6]}),
        json!({"children":[2,9,12]}),
        json!({}),
        json!({"children":[4]}),
        json!({"children":[5]}),
        json!({}),
        json!({"children":[7]}),
        json!({"children":[8]}),
        json!({}),
        json!({"children":[10]}),
        json!({"children":[11]}),
        json!({}),
        json!({"children":[13]}),
        json!({"children":[14]}),
        json!({}),
    ];
    let human_bones = json!({
        "hips":{"node":0},"spine":{"node":1},"head":{"node":2},
        "leftUpperLeg":{"node":3},"leftLowerLeg":{"node":4},"leftFoot":{"node":5},
        "rightUpperLeg":{"node":6},"rightLowerLeg":{"node":7},"rightFoot":{"node":8},
        "leftUpperArm":{"node":9},"leftLowerArm":{"node":10},"leftHand":{"node":11},
        "rightUpperArm":{"node":12},"rightLowerArm":{"node":13},"rightHand":{"node":14}
    });
    let model = json!({
        "asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
        "nodes":nodes,
        "extensions":{"VRMC_vrm":{
            "specVersion":"1.0",
            "meta":{"name":"Test","authors":["Test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"},
            "humanoid":{"humanBones":human_bones}
        }}
    });
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
    let humanoid = model["extensions"]["VRMC_vrm"]["humanoid"].clone();
    motion_value["extensions"] =
        json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":humanoid}});
    motion_value["buffers"] = json!([{"byteLength":32,"uri":"motion.bin"}]);
    motion_value["bufferViews"] =
        json!([{"buffer":0,"byteLength":8},{"buffer":0,"byteOffset":8,"byteLength":24}]);
    motion_value["accessors"] = json!([{"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},{"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}]);
    motion_value["animations"] = json!([{"samplers":[{"input":0,"output":1}],"channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]}]);
    std::fs::write(&motion, motion_value.to_string()).unwrap();
    std::fs::write(&bad_motion, "{}").unwrap();
    let mut app = native_app();
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
fn native_pause_holds_constraint_spring_pose() {
    let directory = std::env::temp_dir().join(format!("ene-avatar-pause-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let nodes = vec![
        json!({"children":[1,3,6]}),
        json!({"children":[2,9,12]}),
        json!({"children":[15]}),
        json!({"children":[4]}),
        json!({"children":[5]}),
        json!({}),
        json!({"children":[7]}),
        json!({"children":[8]}),
        json!({}),
        json!({"children":[10]}),
        json!({"children":[11]}),
        json!({}),
        json!({"children":[13]}),
        json!({"children":[14]}),
        json!({}),
        json!({"translation":[0.2,0.0,0.0]}),
    ];
    let human_bones = json!({
        "hips":{"node":0},"spine":{"node":1},"head":{"node":2},
        "leftUpperLeg":{"node":3},"leftLowerLeg":{"node":4},"leftFoot":{"node":5},
        "rightUpperLeg":{"node":6},"rightLowerLeg":{"node":7},"rightFoot":{"node":8},
        "leftUpperArm":{"node":9},"leftLowerArm":{"node":10},"leftHand":{"node":11},
        "rightUpperArm":{"node":12},"rightLowerArm":{"node":13},"rightHand":{"node":14}
    });
    let mut model = json!({
        "asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
        "nodes":nodes,
        "extensions":{
            "VRMC_vrm":{
                "specVersion":"1.0",
                "meta":{"name":"Test","authors":["Test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"},
                "humanoid":{"humanBones":human_bones}
            },
            "VRMC_springBone":{
                "specVersion":"1.0",
                "springs":[{"joints":[
                    {"node":2,"gravityPower":1.0,"stiffness":0.5,"dragForce":0.4},
                    {"node":15}
                ]}]
            }
        }
    });
    model["nodes"][2]["extensions"] = json!({
        "VRMC_node_constraint": {
            "specVersion": "1.0",
            "constraint": {"rotation": {"source": 1}}
        }
    });
    let path = directory.join("pause.vrm");
    std::fs::write(&path, model.to_string()).unwrap();

    let mut app = native_app();
    app.world_mut().spawn(Output {
        name: "test".into(),
        origin: Vec2::ZERO,
        size: Vec2::splat(100.),
        units_per_logical_pixel: 1.,
    });
    let avatar = app
        .world_mut()
        .spawn((
            CharacterId("pause".into()),
            Config {
                model: path.to_str().unwrap().into(),
                ..default()
            },
        ))
        .id();
    app.world_mut()
        .run_system_cached_with(reload, avatar)
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        app.update();
        let ready = app.world().get::<AvatarState>(avatar) == Some(&AvatarState::Ready);
        let rig = {
            let mut query = app.world_mut().query_filtered::<Entity, With<SpringRig>>();
            query.iter(app.world()).next().is_some()
        };
        if ready && rig {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "paused-model test did not become ready"
        );
        std::thread::yield_now();
    }

    let (root, head) = {
        let mut query = app.world_mut().query::<(Entity, &Vrm)>();
        let (root, vrm) = query.single(app.world()).unwrap();
        (root, vrm.bone(HumanBoneName::Head).expect("head bone"))
    };
    let mut simulated = Quat::IDENTITY;
    for _ in 0..120 {
        app.update();
        let rotation = app.world().get::<Transform>(head).unwrap().rotation;
        if rotation.angle_between(Quat::IDENTITY) > 0.01 {
            simulated = rotation;
            break;
        }
    }
    assert!(
        simulated.angle_between(Quat::IDENTITY) > 0.01,
        "spring joint did not move under the constraint"
    );

    app.world_mut().get_mut::<Config>(avatar).unwrap().playing = false;
    app.update();
    assert!(app.world().get::<SpringRig>(root).unwrap().paused);
    assert!(app.world().get::<SpringJointProps>(head).is_some());
    for _ in 0..10 {
        app.update();
        let held = app.world().get::<Transform>(head).unwrap().rotation;
        assert!(
            held.abs_diff_eq(simulated, 1e-5),
            "playing=false changed the spring pose: {held:?} != {simulated:?}"
        );
        assert!(app.world().get::<SpringRig>(root).unwrap().paused);
    }

    app.world_mut().get_mut::<Config>(avatar).unwrap().playing = true;
    app.update();
    assert!(!app.world().get::<SpringRig>(root).unwrap().paused);
    assert!(app.world().get::<SpringJointProps>(head).is_some());

    app.world_mut().entity_mut(avatar).despawn();
    app.update();
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
            ready(),
            SpringRig::default(),
            Config {
                playing,
                looping: true,
                ..default()
            },
        ));
        actors.push((root, players, spring));
    }

    world.run_system_once(update_avatars).unwrap();
    for (index, (root, players, spring)) in actors.iter().enumerate() {
        for &player in players {
            let player = world.get::<AnimationPlayer>(player).unwrap();
            assert_eq!(player.all_paused(), index == 0);
            assert_eq!(
                player.playing_animations().next().unwrap().1.seek_time(),
                1.25
            );
        }
        assert_eq!(world.get::<SpringRig>(*root).unwrap().paused, index == 0);
        assert!(world.get::<SpringJointProps>(*spring).is_some());
    }
    world.get_mut::<Config>(actors[0].0).unwrap().playing = true;
    world.run_system_once(update_avatars).unwrap();
    assert!(
        !world
            .get::<AnimationPlayer>(actors[0].1[0])
            .unwrap()
            .all_paused()
    );
    assert!(!world.get::<SpringRig>(actors[0].0).unwrap().paused);
    assert!(world.get::<SpringJointProps>(actors[0].2).is_some());
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
            ready(),
            SpringRig::default(),
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
    let late_rig = app
        .world_mut()
        .spawn((SpringRig::default(), ChildOf(avatar)))
        .id();
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
    assert!(app.world().get::<SpringRig>(late_rig).unwrap().paused);
    assert!(app.world().get::<SpringJointProps>(spring).is_some());
    let other = app
        .world_mut()
        .spawn((CharacterId("b".into()), AvatarState::Ready, ready()))
        .id();
    app.world_mut().entity_mut(late).insert(ChildOf(other));
    app.world_mut().entity_mut(late_rig).insert(ChildOf(other));
    app.update();
    assert!(
        !app.world()
            .get::<AnimationPlayer>(late)
            .unwrap()
            .all_paused()
    );
    assert!(!app.world().get::<SpringRig>(late_rig).unwrap().paused);
    assert!(app.world().get::<SpringJointProps>(spring).is_some());
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
            ready(),
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
        .spawn((CharacterId("b".into()), AvatarState::Ready, ready()))
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

    app.world_mut().entity_mut(avatar).insert(ready());
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
