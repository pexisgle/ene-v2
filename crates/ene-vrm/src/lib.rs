//! VRM 1.0 extensions for Bevy's native glTF, scene, material and animation APIs.
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

mod animation;
mod expressions;
mod loader;
mod material;
mod runtime;
mod schema;

use bevy::{app::AnimationSystems, prelude::*, transform::TransformSystems};
pub use runtime::{SpringJointProps, SpringRig, Vrm};
pub use vrm_spec::vrmc_vrm_1_0::{HumanBoneName, Meta};

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VrmSystems {
    Pose,
    LookAt,
    Expressions,
    Constraints,
    SpringBone,
}

pub struct VrmPlugin;

impl Plugin for VrmPlugin {
    fn build(&self, app: &mut App) {
        loader::setup(app);
        material::setup(app);
        app.configure_sets(
            PostUpdate,
            (
                VrmSystems::Pose,
                VrmSystems::LookAt,
                VrmSystems::Expressions,
                VrmSystems::Constraints,
                VrmSystems::SpringBone,
            )
                .chain()
                .after(AnimationSystems)
                .before(TransformSystems::Propagate),
        );
        animation::setup(app);
        expressions::setup(app);
        runtime::setup(app);
    }
    fn finish(&self, app: &mut App) {
        loader::finish(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        animation::{AnimatedBy, AnimationTargetId},
        asset::LoadState,
        gltf::Gltf,
    };
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };

    struct Files(tempfile::TempDir);
    impl Files {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("create test fixture directory"))
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.path().join(name)
        }
        fn write(&self, name: &str, value: &Value) -> String {
            let path = self.join(name);
            std::fs::write(&path, value.to_string()).unwrap();
            path.to_str().unwrap().to_owned()
        }
        fn model(&self) -> Value {
            json!({
                "asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0]}],
                "nodes":[
                    {"name":"Same","children":[1]},
                    {"name":"Same","translation":[0,1,0],"children":[2,4,7]},
                    {"name":"Same","translation":[0,1,0],"children":[3,10,13]},
                    {"name":"Head"},
                    {"name":"LeftUpperLeg","children":[5]},
                    {"name":"LeftLowerLeg","children":[6]},
                    {"name":"LeftFoot"},
                    {"name":"RightUpperLeg","children":[8]},
                    {"name":"RightLowerLeg","children":[9]},
                    {"name":"RightFoot"},
                    {"name":"LeftUpperArm","children":[11]},
                    {"name":"LeftLowerArm","children":[12]},
                    {"name":"LeftHand"},
                    {"name":"RightUpperArm","children":[14]},
                    {"name":"RightLowerArm","children":[15]},
                    {"name":"RightHand"}
                ],
                "extensions":{"VRMC_vrm":{
                    "specVersion":"1.0",
                    "meta":{"name":"Test","authors":["Test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"},
                    "humanoid":{"humanBones":{
                        "hips":{"node":1},"spine":{"node":2},"head":{"node":3},
                        "leftUpperLeg":{"node":4},"leftLowerLeg":{"node":5},"leftFoot":{"node":6},
                        "rightUpperLeg":{"node":7},"rightLowerLeg":{"node":8},"rightFoot":{"node":9},
                        "leftUpperArm":{"node":10},"leftLowerArm":{"node":11},"leftHand":{"node":12},
                        "rightUpperArm":{"node":13},"rightLowerArm":{"node":14},"rightHand":{"node":15}
                    }},
                    "expressions":{"preset":{"aa":{},"happy":{"overrideMouth":"block"}}}
                }}
            })
        }
        fn motion(&self, interpolation: &str) -> Value {
            let values: [f32; 16] = [
                0., 1., 0., 1., 0., 1., 1., 0., 0., 0., 0., 1., 0.19866933, 0., 0., 0.9800666,
            ];
            std::fs::write(
                self.join("motion.bin"),
                values
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let mut v = self.model();
            let humanoid = v["extensions"]["VRMC_vrm"]["humanoid"].clone();
            v["extensions"] =
                json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":humanoid}});
            v["buffers"] = json!([{"byteLength":64,"uri":"motion.bin"}]);
            v["bufferViews"] = json!([
                {"buffer":0,"byteLength":8},
                {"buffer":0,"byteOffset":8,"byteLength":24},
                {"buffer":0,"byteOffset":32,"byteLength":32}
            ]);
            v["accessors"] = json!([
                {"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},
                {"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"},
                {"bufferView":2,"componentType":5126,"count":2,"type":"VEC4"}
            ]);
            v["animations"] = json!([{
                "samplers":[
                    {"input":0,"output":1,"interpolation":interpolation},
                    {"input":0,"output":2,"interpolation":interpolation}
                ],
                "channels":[
                    {"sampler":0,"target":{"node":1,"path":"translation"}},
                    {"sampler":1,"target":{"node":2,"path":"rotation"}}
                ]
            }]);
            v
        }
    }

    #[derive(Resource, Default)]
    struct BeforeAnimation(bevy::ecs::change_detection::Tick);

    fn app() -> App {
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
        .add_plugins(bevy::extract::sync_world::SyncWorldPlugin::<
            bevy::render::RenderApp,
        >::default())
        .register_asset_loader(bevy::image::ImageLoader::new(
            bevy::image::CompressedImageFormats::NONE,
        ))
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_secs_f64(1. / 60.),
        ))
        .add_plugins(VrmPlugin)
        .init_resource::<BeforeAnimation>()
        .add_systems(
            PostUpdate,
            (|mut b: ResMut<BeforeAnimation>, ticks: bevy::ecs::system::SystemChangeTick| {
                b.0 = ticks.this_run();
            })
            .before(bevy::app::AnimationSystems),
        );
        app.finish();
        app.cleanup();
        app
    }

    fn settle<A: Asset>(app: &mut App, handles: &[Handle<A>]) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            app.update();
            let server = app.world().resource::<AssetServer>();
            if handles.iter().all(|h| {
                matches!(server.get_load_state(h.id()), Some(LoadState::Failed(_)))
                    || server.is_loaded_with_dependencies(h.id())
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "load did not terminate");
            std::thread::yield_now();
        }
    }

    fn gaze_input(app: &App, root: Entity) -> Entity {
        app.world()
            .iter_entities()
            .find(|e| {
                e.get::<AnimationTargetId>() == Some(&animation::gaze_target())
                    && e.get::<AnimatedBy>().is_some_and(|p| p.0 == root)
            })
            .unwrap()
            .id()
    }

    fn gaze_input_active(app: &App, root: Entity) -> bool {
        app.world()
            .entity(gaze_input(app, root))
            .get_ref::<Transform>()
            .unwrap()
            .last_changed()
            .is_newer_than(
                app.world().resource::<BeforeAnimation>().0,
                app.world().read_change_tick(),
            )
    }
    #[test]
    fn vrm_spec_system_order_runs_pose_lookat_expressions_constraints_springbone() {
        let mut app = app();
        let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        for (set, name) in [
            (VrmSystems::Pose, "pose"),
            (VrmSystems::LookAt, "lookAt"),
            (VrmSystems::Expressions, "expressions"),
            (VrmSystems::Constraints, "constraints"),
            (VrmSystems::SpringBone, "springBone"),
        ] {
            let log = log.clone();
            app.add_systems(
                PostUpdate,
                (move || log.lock().unwrap().push(name)).in_set(set),
            );
        }
        app.update();
        assert_eq!(
            *log.lock().unwrap(),
            vec!["pose", "lookAt", "expressions", "constraints", "springBone"]
        );
    }
    #[test]
    fn loader_rejects_missing_extensions_and_dangling_bones() {
        let files = Files::new();
        let mut app = app();
        let mut missing = files.model();
        missing["extensions"] = json!({});
        let mut dangling = files.model();
        dangling["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["hips"]["node"] = json!(99);
        for (name, value) in [("missing.vrm", missing), ("dangling.vrm", dangling)] {
            let handle: Handle<Gltf> = app
                .world()
                .resource::<AssetServer>()
                .load(files.write(name, &value));
            settle(&mut app, std::slice::from_ref(&handle));
            assert!(
                matches!(
                    app.world()
                        .resource::<AssetServer>()
                        .get_load_state(handle.id()),
                    Some(LoadState::Failed(_))
                ),
                "{name} must fail"
            );
        }
    }

    #[test]
    fn scene_instances_remap_bones_and_preserve_native_and_vrma_playback() {
        let files = Files::new();
        let mut app = app();
        let mut model = files.model();
        model["scenes"] = json!([{"nodes":[0]},{"nodes":[0]}]);
        model["scene"] = json!(1);
        let eye = model["nodes"].as_array().unwrap().len();
        model["nodes"][3]["children"] = json!([eye, eye + 1]);
        model["nodes"].as_array_mut().unwrap().extend([
            json!({"name":"LeftEye","translation":[0.1,0.1,0.],"rotation":Quat::from_rotation_z(0.15).to_array()}),
            json!({"name":"RightEye","translation":[-0.1,0.1,0.]})
        ]);
        model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["leftEye"] = json!({"node":eye});
        model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["rightEye"] =
            json!({"node":eye + 1});
        model["extensions"]["VRMC_vrm"]["lookAt"] =
            json!({"type":"bone","offsetFromHeadBone":[0.02,0.03,0.04]});
        model["extensions"]["VRMC_vrm"]["expressions"]["preset"]["blink"] =
            json!({"overrideLookAt":"blend"});

        let mut motion_val = files.motion("LINEAR");
        motion_val["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
        motion_val["animations"][0]["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"sampler":1,"target":{"node":0,"path":"rotation"}}));

        let h_model: Handle<Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load(files.write("model.vrm", &model));
        let h_motion: Handle<Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load(files.write("motion.vrma", &motion_val));
        settle(&mut app, &[h_model.clone(), h_motion.clone()]);

        let scene = app
            .world()
            .resource::<Assets<Gltf>>()
            .get(&h_model)
            .unwrap()
            .default_scene
            .clone()
            .unwrap();
        let clip = app
            .world()
            .resource::<Assets<Gltf>>()
            .get(&h_motion)
            .unwrap()
            .animations[0]
            .clone();
        let (graph, node) = AnimationGraph::from_clip(clip);
        let graph = app
            .world_mut()
            .resource_mut::<Assets<AnimationGraph>>()
            .add(graph);

        let holders: Vec<_> = [
            Transform::IDENTITY,
            Transform::from_xyz(20., -30., 7.)
                .with_rotation(Quat::from_rotation_y(0.7))
                .with_scale(Vec3::splat(160.)),
        ]
        .into_iter()
        .map(|t| {
            app.world_mut()
                .spawn((WorldAssetRoot(scene.clone()), t))
                .id()
        })
        .collect();
        for _ in 0..5 {
            app.update();
        }

        assert_eq!(
            app.world_mut()
                .query::<&loader::VrmScene>()
                .iter(app.world())
                .count(),
            0
        );
        let rigs: Vec<_> = app
            .world_mut()
            .query::<(Entity, &Vrm)>()
            .iter(app.world())
            .map(|(e, v)| {
                (
                    e,
                    v.bone(HumanBoneName::Hips).unwrap(),
                    v.bone(HumanBoneName::Spine).unwrap(),
                    v.bone(HumanBoneName::Head).unwrap(),
                    v.bone(HumanBoneName::LeftEye).unwrap(),
                )
            })
            .collect();
        assert_eq!(rigs.len(), 2);
        assert_eq!(
            app.world().get::<Vrm>(rigs[0].0).unwrap().meta.name,
            "Test",
            "Vrm must expose the parsed VRM meta"
        );
        let bone_count: usize = app
            .world_mut()
            .query::<&Vrm>()
            .iter(app.world())
            .map(|vrm| vrm.bones.len())
            .sum();
        assert_eq!(
            app.world_mut()
                .query::<&animation::Pose>()
                .iter(app.world())
                .count(),
            bone_count,
            "humanoid bones own Pose directly; no per-bone target entities"
        );
        for &(root, hips, spine, head, left_eye) in &rigs {
            assert_eq!(
                app.world().get::<AnimationTargetId>(hips),
                Some(&animation::bone_target(HumanBoneName::Hips))
            );
            for bone in [hips, spine, head, left_eye] {
                assert!(app.world().get::<animation::Pose>(bone).is_some());
                assert!(
                    app.world()
                        .get::<AnimatedBy>(bone)
                        .is_some_and(|by| by.0 == root)
                );
            }
        }

        for &(root, _, _, _, _) in &rigs {
            let mut player = AnimationPlayer::default();
            player.play(node).seek_to(0.5).pause();
            app.world_mut()
                .entity_mut(root)
                .insert((player, AnimationGraphHandle(graph.clone())));
        }
        for _ in 0..2 {
            app.update();
        }
        for &(root, hips, spine, _, _) in &rigs {
            assert!(
                app.world()
                    .get::<Transform>(hips)
                    .unwrap()
                    .translation
                    .abs_diff_eq(Vec3::new(0.5, 1., 0.), 1e-4)
            );
            assert!(
                app.world()
                    .get::<Transform>(spine)
                    .unwrap()
                    .rotation
                    .abs_diff_eq(Quat::from_rotation_x(0.2), 1e-4)
            );
            assert!(gaze_input_active(&app, root));
        }
        let gaze_eye_rest = app
            .world()
            .get::<animation::RestPose>(rigs[0].4)
            .unwrap()
            .local
            .rotation;
        assert!(
            app.world()
                .get::<Transform>(rigs[0].4)
                .unwrap()
                .rotation
                .angle_between(gaze_eye_rest)
                > 0.01,
            "VRMA gaze did not rotate the eye bone"
        );

        // A stopped animation leaves every humanoid bone at rest.
        let (root, hips, spine, _, eye) = rigs[0];
        let rest = [("hips", hips), ("spine", spine), ("eye", eye)].map(|(name, entity)| {
            (
                name,
                entity,
                app.world()
                    .get::<animation::RestPose>(entity)
                    .unwrap()
                    .local,
            )
        });
        let eye_rest = rest[2].2.rotation;
        app.world_mut()
            .get_mut::<AnimationPlayer>(root)
            .unwrap()
            .stop_all();
        app.update();
        assert!(!gaze_input_active(&app, root));
        for (name, entity, expected) in rest {
            let actual = *app.world().get::<Transform>(entity).unwrap();
            assert!(
                actual.rotation.abs_diff_eq(expected.rotation, 1e-5)
                    && actual.translation.abs_diff_eq(expected.translation, 1e-5),
                "{name} did not return to rest: {actual:?} != {expected:?}"
            );
        }
        assert!(
            app.world()
                .get::<Transform>(eye)
                .unwrap()
                .rotation
                .abs_diff_eq(eye_rest, 1e-5)
        );

        for h in holders {
            app.world_mut().entity_mut(h).despawn();
        }
        app.update();
        assert_eq!(app.world_mut().query::<&Vrm>().iter(app.world()).count(), 0);
    }
    #[test]
    fn native_scene_instances_remap_one_shared_collider_for_multiple_chains() {
        let files = Files::new();
        let mut value = files.model();
        let base = value["nodes"].as_array().unwrap().len();
        value["nodes"][1]["children"] = serde_json::json!([2, 4, 7, base, base + 2]);
        value["nodes"].as_array_mut().unwrap().extend([
            serde_json::json!({"translation":[0.1,1.,0.],"children":[base + 1]}),
            serde_json::json!({"translation":[0.,0.1,0.]}),
            serde_json::json!({"translation":[-0.1,1.,0.],"children":[base + 3]}),
            serde_json::json!({"translation":[0.,0.1,0.]}),
        ]);
        value["extensions"]["VRMC_springBone"] = serde_json::json!({
            "specVersion":"1.0",
            "colliders":[{"node":1,"shape":{"sphere":{"offset":[0.,10.,0.],"radius":0.01}}}],
            "colliderGroups":[{"colliders":[0]}],
            "springs":[
                {"joints":[{"node":base},{"node":base + 1}],"colliderGroups":[0]},
                {"joints":[{"node":base + 2},{"node":base + 3}],"colliderGroups":[0]}
            ]
        });
        let mut app = app();
        let handle: Handle<bevy::gltf::Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load(files.write("shared-collider.vrm", &value));
        settle(&mut app, std::slice::from_ref(&handle));
        let scene = app
            .world()
            .resource::<Assets<bevy::gltf::Gltf>>()
            .get(&handle)
            .unwrap()
            .default_scene
            .clone()
            .unwrap();
        let holders: Vec<_> = [0., 20.]
            .into_iter()
            .map(|x| {
                app.world_mut()
                    .spawn((
                        WorldAssetRoot(scene.clone()),
                        Transform::from_xyz(x, 0., 0.),
                    ))
                    .id()
            })
            .collect();
        for _ in 0..5 {
            app.update();
        }
        let mut q = app.world_mut().query::<(&Vrm, &SpringRig)>();
        let rigs: Vec<_> = q.iter(app.world()).collect();
        assert_eq!(rigs.len(), 2);
        for (vrm, rig) in &rigs {
            assert_eq!(
                rig.colliders[0].entity,
                vrm.bone(crate::schema::HumanBoneName::Hips).unwrap()
            );
            assert_eq!(rig.chains.len(), 2);
            for chain in &rig.chains {
                assert_eq!(chain.colliders, vec![0]);
                assert!(chain.joints[0].state.unwrap().iter().all(|p| p.is_finite()));
                assert!(
                    app.world()
                        .get::<SpringJointProps>(chain.joints[0].entity)
                        .is_some()
                );
            }
        }
        assert_ne!(
            rigs[0].1.chains[0].joints[0].entity,
            rigs[1].1.chains[0].joints[0].entity
        );
        app.world_mut().entity_mut(holders[0]).despawn();
        for _ in 0..3 {
            app.update();
        }
        let mut q = app.world_mut().query::<&SpringRig>();
        let rig = q.single(app.world()).unwrap();
        assert!(rig.chains[0].joints[0].state.is_some());
    }

    #[test]
    fn paused_spring_holds_last_pose() {
        let files = Files::new();
        let mut app = app();
        let mut model = files.model();
        let head = 3;
        let tail = model["nodes"].as_array().unwrap().len();
        model["nodes"][head]["children"] = json!([tail]);
        model["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"translation":[0.2,0.0,0.0]}));
        model["nodes"][head]["extensions"] = json!({
            "VRMC_node_constraint": {
                "specVersion":"1.0",
                "constraint":{"rotation":{"source":2}}
            }
        });
        model["extensions"]["VRMC_springBone"] = json!({
            "specVersion":"1.0",
            "springs":[{"joints":[
                {"node":head,"gravityPower":1.0,"stiffness":0.5,"dragForce":0.4},
                {"node":tail}
            ]}]
        });

        let handle: Handle<Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load(files.write("pause.vrm", &model));
        settle(&mut app, std::slice::from_ref(&handle));
        let scene = app
            .world()
            .resource::<Assets<Gltf>>()
            .get(&handle)
            .unwrap()
            .default_scene
            .clone()
            .unwrap();
        app.world_mut()
            .spawn((WorldAssetRoot(scene), Transform::default()));

        let (root, head_entity) = loop {
            app.update();
            let mut query = app.world_mut().query::<(Entity, &Vrm)>();
            if let Some((root, vrm)) = query.iter(app.world()).next() {
                break (root, vrm.bone(HumanBoneName::Head).expect("head bone"));
            }
        };
        let mut simulated = Quat::IDENTITY;
        for _ in 0..120 {
            app.update();
            let rotation = app.world().get::<Transform>(head_entity).unwrap().rotation;
            if rotation.angle_between(Quat::IDENTITY) > 0.01 {
                simulated = rotation;
                break;
            }
        }
        assert!(
            simulated.angle_between(Quat::IDENTITY) > 0.01,
            "spring joint did not move"
        );

        app.world_mut().get_mut::<SpringRig>(root).unwrap().paused = true;
        for _ in 0..10 {
            app.update();
            let held = app.world().get::<Transform>(head_entity).unwrap().rotation;
            assert!(
                held.abs_diff_eq(simulated, 1e-5),
                "paused spring moved: {held:?} != {simulated:?}"
            );
        }
        app.world_mut().get_mut::<SpringRig>(root).unwrap().paused = false;
        app.update();
        assert!(!app.world().get::<SpringRig>(root).unwrap().paused);
    }
}
