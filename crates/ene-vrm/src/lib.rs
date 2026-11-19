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
pub use expressions::ExpressionWeights;
pub use runtime::{GazeOverride, SpringJointProps, SpringRig, Vrm};
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
                "extensionsUsed":["VRMC_vrm"],"extensionsRequired":["VRMC_vrm"],
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
            v["extensionsUsed"] = json!(["VRMC_vrm_animation"]);
            v["extensionsRequired"] = json!(["VRMC_vrm_animation"]);
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

    fn app() -> App {
        let plugins = DefaultPlugins.build().disable::<bevy::log::LogPlugin>();
        #[cfg(windows)]
        let plugins = plugins.disable::<bevy::winit::WinitPlugin>();
        let mut app = App::new();
        app.add_plugins(
            plugins
                .set(AssetPlugin {
                    unapproved_path_mode: bevy::asset::UnapprovedPathMode::Allow,
                    ..default()
                })
                .set(WindowPlugin {
                    primary_window: None,
                    ..default()
                })
                .set(bevy::render::RenderPlugin {
                    render_creation: bevy::render::settings::WgpuSettings {
                        backends: None,
                        ..default()
                    }
                    .into(),
                    ..default()
                }),
        )
        .add_plugins(bevy::extract::sync_world::SyncWorldPlugin::<
            bevy::render::RenderApp,
        >::default())
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_secs_f64(1. / 60.),
        ))
        .add_plugins(VrmPlugin);
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
            .get::<animation::GazeInput>()
            .unwrap()
            .rotation
            .is_some()
    }
    #[test]
    fn loader_rejects_missing_extensions_dangling_bones_and_unknown_requirements() {
        let files = Files::new();
        let mut app = app();
        let mut missing = files.model();
        missing["extensions"] = json!({});
        let mut dangling = files.model();
        dangling["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["hips"]["node"] = json!(99);
        let mut unknown = files.model();
        unknown["extensionsRequired"] = json!(["VRMC_vrm", "ENE_unsupported"]);
        unknown["extensionsUsed"] = unknown["extensionsRequired"].clone();
        let mut malformed_uv = files.model();
        malformed_uv["materials"] = json!([{"extensions":{"VRMC_materials_mtoon":{
            "specVersion":"1.0", "shadingShiftTexture":{
                "index":0,"extensions":{"KHR_texture_transform":{"scale":[1]}}
            }
        }}}]);
        for (name, value) in [
            ("missing.vrm", missing),
            ("dangling.vrm", dangling),
            ("unknown.vrm", unknown),
            ("malformed-uv.vrm", malformed_uv),
        ] {
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
    fn native_motion_identities_cover_unnamed_and_cross_scene_collisions() {
        let files = Files::new();
        let base = files.motion("LINEAR");
        let mut unnamed = base.clone();
        for node in unnamed["nodes"].as_array_mut().unwrap() {
            node.as_object_mut().unwrap().remove("name");
        }
        unnamed["scenes"] = json!([{"nodes":[0]}, {"nodes":[0]}]);
        unnamed["scene"] = json!(1);
        let mut collision = base.clone();
        collision["nodes"].as_array_mut().unwrap().extend([
            json!({"name":"Same","children":[17]}),
            json!({"name":"Same"}),
        ]);
        collision["scenes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nodes":[16]}));
        collision["animations"][0]["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"sampler":0,"target":{"node":17,"path":"translation"}}));
        let mut different_property = collision.clone();
        different_property["animations"][0]["channels"][2] =
            json!({"sampler":1,"target":{"node":17,"path":"rotation"}});
        let mut changed_path = base.clone();
        changed_path["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"Different","children":[1]}));
        changed_path["scenes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nodes":[16]}));
        let mut app = app();
        for (name, value, succeeds) in [
            ("unnamed.vrma", unnamed, true),
            ("collision.vrma", collision, false),
            ("different-property.vrma", different_property, true),
            ("changed-path.vrma", changed_path, false),
        ] {
            let handle: Handle<Gltf> = app
                .world()
                .resource::<AssetServer>()
                .load(files.write(name, &value));
            settle(&mut app, std::slice::from_ref(&handle));
            assert_eq!(
                app.world().resource::<Assets<Gltf>>().contains(&handle),
                succeeds,
                "{name}: {:?}",
                app.world()
                    .resource::<AssetServer>()
                    .get_load_state(handle.id())
            );
        }
    }

    #[test]
    fn review_skipped_channels_preserve_baseline_target_rejection_order() {
        let files = Files::new();
        let mut base = files.motion("LINEAR");
        let mut bytes = std::fs::read(files.join("motion.bin")).unwrap();
        bytes.extend([0, 0, 0, 0]);
        bytes.extend(0f32.to_le_bytes());
        std::fs::write(files.join("motion.bin"), bytes).unwrap();
        base["buffers"][0]["byteLength"] = json!(72);
        base["bufferViews"].as_array_mut().unwrap().extend([
            json!({"buffer":0,"byteOffset":64,"byteLength":4}),
            json!({"buffer":0,"byteOffset":68,"byteLength":4}),
        ]);
        base["accessors"].as_array_mut().unwrap().push(json!({
            "bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1],
            "sparse":{"count":1,"indices":{"bufferView":3,"componentType":5121},"values":{"bufferView":4}}
        }));
        base["animations"][0]["samplers"][0]["input"] = json!(3);
        base["animations"][0]["channels"][1]["target"]["node"] = json!(0);
        base["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
        let mut changed = base.clone();
        changed["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"Different","children":[1]}));
        changed["scenes"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nodes":[16]}));
        let mut ignored = changed.clone();
        ignored["animations"][0]["channels"][0]["target"]["node"] = json!(4);
        let mut unsupported = changed.clone();
        unsupported["animations"][0]["channels"][0]["target"]["path"] = json!("scale");
        let mut app = app();
        for (name, value, expected) in [
            ("sparse-same-path.vrma", base, true),
            ("sparse-changed-path.vrma", changed, false),
            ("sparse-ignored-target.vrma", ignored, true),
            ("sparse-unsupported-property.vrma", unsupported, true),
        ] {
            let handle: Handle<Gltf> = app
                .world()
                .resource::<AssetServer>()
                .load(files.write(name, &value));
            settle(&mut app, std::slice::from_ref(&handle));
            let state = app
                .world()
                .resource::<AssetServer>()
                .get_load_state(handle.id());
            let loaded = app.world().resource::<Assets<Gltf>>().contains(&handle);
            eprintln!("review fixture {name}: loaded={loaded}, state={state:?}");
            assert_eq!(loaded, expected, "{name}: {state:?}");
        }
    }

    #[test]
    fn review_mtoon_texture_presence_and_malformed_fields_use_raw_boundary() {
        let files = Files::new();
        let mut app = app();
        for slot in [
            "shadeMultiplyTexture",
            "shadingShiftTexture",
            "rimMultiplyTexture",
            "matcapTexture",
            "uvAnimationMaskTexture",
            "outlineWidthMultiplyTexture",
        ] {
            for (case, info, expected) in [
                ("omitted", json!({"index":0}), true),
                ("null-texcoord", json!({"index":0,"texCoord":null}), false),
                ("null-index", json!({"index":null}), false),
                ("missing-index", json!({"texCoord":0}), false),
                ("negative-texcoord", json!({"index":0,"texCoord":-1}), false),
                (
                    "null-scale",
                    json!({"index":0,"extensions":{"KHR_texture_transform":{"scale":null}}}),
                    false,
                ),
                (
                    "short-offset",
                    json!({"index":0,"extensions":{"KHR_texture_transform":{"offset":[0]}}}),
                    false,
                ),
                (
                    "null-override",
                    json!({"index":0,"extensions":{"KHR_texture_transform":{"texCoord":null}}}),
                    true,
                ),
            ] {
                let mut value = files.model();
                let mut toon = json!({"specVersion":"1.0"});
                toon[slot] = info;
                value["materials"] = json!([{"extensions":{"VRMC_materials_mtoon":toon}}]);
                let name = format!("{slot}-{case}.vrm");
                let handle: Handle<Gltf> = app
                    .world()
                    .resource::<AssetServer>()
                    .load(files.write(&name, &value));
                settle(&mut app, std::slice::from_ref(&handle));
                let state = app
                    .world()
                    .resource::<AssetServer>()
                    .get_load_state(handle.id());
                let loaded = app.world().resource::<Assets<Gltf>>().contains(&handle);
                eprintln!("review fixture {name}: loaded={loaded}, state={state:?}");
                assert_eq!(loaded, expected, "{name}: {state:?}");
            }
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
        motion_val["extensions"]["VRMC_vrm_animation"]["expressions"] =
            json!({"preset":{"aa":{"node":4}}});
        motion_val["animations"][0]["channels"]
            .as_array_mut()
            .unwrap()
            .push(json!({"sampler":0,"target":{"node":4,"path":"translation"}}));
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
        assert!(
            app.world().resource::<Assets<Gltf>>().contains(&h_motion),
            "{:?}",
            app.world()
                .resource::<AssetServer>()
                .get_load_state(h_motion.id())
        );

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
            app.world_mut()
                .query_filtered::<Entity, Or<(
                    With<animation::NativeNode>,
                    With<animation::NativeSceneRoot>
                )>>()
                .iter(app.world())
                .count(),
            0
        );
        for &(root, _, _, head, _) in &rigs {
            app.world_mut()
                .entity_mut(head)
                .insert(bevy::mesh::morph::MorphWeights::new(vec![0.], None).unwrap());
            let mut binds = app.world_mut().get_mut::<expressions::Binds>(root).unwrap();
            let index = binds
                .bindings
                .iter()
                .position(|binding| {
                    binding.name
                        == expressions::ExpressionName::Preset(schema::ExpressionPresetName::Aa)
                })
                .unwrap();
            let target = binds.bindings[index].target;
            binds.morphs.push(expressions::MorphTarget {
                entity: head,
                indices: vec![(0, vec![(index, 1.)])],
            });
            assert!(app.world().get::<Transform>(target).is_none());
        }
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
            assert!(
                app.world()
                    .get::<Transform>(gaze_input(&app, root))
                    .is_none()
            );
            let head = app
                .world()
                .get::<Vrm>(root)
                .unwrap()
                .bone(HumanBoneName::Head)
                .unwrap();
            assert!(
                (app.world()
                    .get::<bevy::mesh::morph::MorphWeights>(head)
                    .unwrap()
                    .weights()[0]
                    - 0.5)
                    .abs()
                    < 1e-5
            );
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

        let morph = |app: &App, head| {
            app.world()
                .get::<bevy::mesh::morph::MorphWeights>(head)
                .unwrap()
                .weights()[0]
        };
        assert_eq!(morph(&app, rigs[0].3), 0.);
        assert!((morph(&app, rigs[1].3) - 0.5).abs() < 1e-5);
        assert!(gaze_input_active(&app, rigs[1].0));
        app.world_mut()
            .get_mut::<AnimationPlayer>(rigs[1].0)
            .unwrap()
            .animation_mut(node)
            .unwrap()
            .set_weight(0.);
        app.update();
        assert_eq!(morph(&app, rigs[1].3), 0.);
        assert!(!gaze_input_active(&app, rigs[1].0));
        app.world_mut()
            .get_mut::<AnimationPlayer>(rigs[1].0)
            .unwrap()
            .animation_mut(node)
            .unwrap()
            .set_weight(1.);
        app.update();
        assert!((morph(&app, rigs[1].3) - 0.5).abs() < 1e-5);
        {
            let mut graphs = app.world_mut().resource_mut::<Assets<AnimationGraph>>();
            let mut graph = graphs.get_mut(&graph).unwrap();
            graph.add_target_to_mask_group(animation::gaze_target(), 0);
            graph.add_target_to_mask_group(
                animation::expression_target(&expressions::ExpressionName::Preset(
                    schema::ExpressionPresetName::Aa,
                )),
                0,
            );
            graph.graph[node].add_mask_group(0);
        }
        for _ in 0..2 {
            app.update();
        }
        assert_eq!(morph(&app, rigs[1].3), 0.);
        assert!(!gaze_input_active(&app, rigs[1].0));
        for h in holders {
            app.world_mut().entity_mut(h).despawn();
        }
        app.update();
        assert_eq!(app.world_mut().query::<&Vrm>().iter(app.world()).count(), 0);
    }
    #[test]
    fn typed_expression_and_gaze_inputs_keep_native_interpolation_and_pause() {
        let mut app = app();
        for interpolation in ["STEP", "LINEAR", "CUBICSPLINE"] {
            let files = Files::new();
            let mut motion = files.motion(interpolation);
            motion["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
            motion["extensions"]["VRMC_vrm_animation"]["expressions"] =
                json!({"preset":{"aa":{"node":4}}});
            motion["animations"][0]["channels"]
                .as_array_mut()
                .unwrap()
                .extend([
                    json!({"sampler":0,"target":{"node":4,"path":"translation"}}),
                    json!({"sampler":1,"target":{"node":0,"path":"rotation"}}),
                ]);
            if interpolation == "CUBICSPLINE" {
                let mut values = vec![0., 1.];
                for value in [[0., 1., 0.], [1., 1., 0.]] {
                    values.extend([0.; 3]);
                    values.extend(value);
                    values.extend([0.; 3]);
                }
                for value in [Quat::IDENTITY, Quat::from_rotation_x(0.4)] {
                    values.extend([0.; 4]);
                    values.extend(value.to_array());
                    values.extend([0.; 4]);
                }
                std::fs::write(
                    files.join("motion.bin"),
                    values
                        .into_iter()
                        .flat_map(f32::to_le_bytes)
                        .collect::<Vec<_>>(),
                )
                .unwrap();
                motion["buffers"][0]["byteLength"] = json!(176);
                motion["bufferViews"][1]["byteLength"] = json!(72);
                motion["bufferViews"][2]["byteOffset"] = json!(80);
                motion["bufferViews"][2]["byteLength"] = json!(96);
                motion["accessors"][1]["count"] = json!(6);
                motion["accessors"][2]["count"] = json!(6);
            }
            let handle: Handle<Gltf> = app
                .world()
                .resource::<AssetServer>()
                .load(files.write("inputs.vrma", &motion));
            settle(&mut app, std::slice::from_ref(&handle));
            let clip = app
                .world()
                .resource::<Assets<Gltf>>()
                .get(&handle)
                .unwrap()
                .animations[0]
                .clone();
            let (graph, node) = AnimationGraph::from_clip(clip);
            let graph = app
                .world_mut()
                .resource_mut::<Assets<AnimationGraph>>()
                .add(graph);
            let mut player = AnimationPlayer::default();
            player.start(node).pause().set_seek_time(0.25);
            let root = app
                .world_mut()
                .spawn((player, AnimationGraphHandle(graph)))
                .id();
            let expression = app
                .world_mut()
                .spawn((
                    animation::ExpressionInput::default(),
                    animation::expression_target(&expressions::ExpressionName::Preset(
                        schema::ExpressionPresetName::Aa,
                    )),
                    AnimatedBy(root),
                    ChildOf(root),
                ))
                .id();
            let gaze = app
                .world_mut()
                .spawn((
                    animation::GazeInput::default(),
                    animation::gaze_target(),
                    AnimatedBy(root),
                    ChildOf(root),
                ))
                .id();
            for _ in 0..2 {
                app.update();
            }
            let expected = match interpolation {
                "STEP" => 0.,
                "LINEAR" => 0.25,
                _ => 0.15625,
            };
            let rotation = match interpolation {
                "STEP" => Quat::IDENTITY,
                "LINEAR" => Quat::from_rotation_x(0.1),
                _ => Quat::from_vec4(
                    Vec4::from_array(Quat::IDENTITY.to_array())
                        .lerp(
                            Vec4::from_array(Quat::from_rotation_x(0.4).to_array()),
                            expected,
                        )
                        .normalize(),
                ),
            };
            for _ in 0..2 {
                app.update();
                assert!(
                    (app.world()
                        .get::<animation::ExpressionInput>(expression)
                        .unwrap()
                        .value
                        .unwrap()
                        - expected)
                        .abs()
                        < 1e-5,
                    "{interpolation}"
                );
                assert!(
                    app.world()
                        .get::<animation::GazeInput>(gaze)
                        .unwrap()
                        .rotation
                        .unwrap()
                        .abs_diff_eq(rotation, 1e-5),
                    "{interpolation}"
                );
            }
            app.world_mut()
                .get_mut::<AnimationPlayer>(root)
                .unwrap()
                .stop_all();
            app.update();
            assert_eq!(
                app.world()
                    .get::<animation::ExpressionInput>(expression)
                    .unwrap()
                    .value,
                None
            );
            assert_eq!(
                app.world()
                    .get::<animation::GazeInput>(gaze)
                    .unwrap()
                    .rotation,
                None
            );
            app.world_mut().entity_mut(root).despawn();
        }
    }

    #[test]
    fn expression_materials_wait_atomically_and_isolate_ready_instances() {
        use crate::material::{BaseUv, EmissiveStrength, MaterialSlot};
        use bevy::{platform::collections::HashMap, world_serialization::WorldInstanceReady};

        #[derive(Resource, Default)]
        struct Ready(usize);

        let mut app = app();
        app.init_resource::<Ready>()
            .add_observer(|_: On<WorldInstanceReady>, mut ready: ResMut<Ready>| ready.0 += 1);
        let (original, missing) = {
            let mut assets = app.world_mut().resource_mut::<Assets<StandardMaterial>>();
            (
                assets.add(StandardMaterial::default()),
                assets.reserve_handle(),
            )
        };
        let definitions = serde_json::from_value(json!({"custom":{"slide":{
            "textureTransformBinds":[{"material":0,"scale":[2,2],"offset":[0.5,0]}]
        }}}))
        .unwrap();
        let uv = schema::UvTransform {
            scale: Vec2::new(0., -2.),
            rotation: -0.5,
            offset: Vec2::new(0.2, 0.4),
        };
        let mut world = World::new();
        let root = world.spawn(Transform::default()).id();
        expressions::attach(&mut world, root, Some(&definitions), &HashMap::default()).unwrap();
        for handle in [&original, &original, &missing] {
            world.spawn((
                MeshMaterial3d(handle.clone()),
                MaterialSlot(0),
                BaseUv(uv),
                EmissiveStrength(2.),
                ChildOf(root),
            ));
        }
        let scene = app
            .world_mut()
            .resource_mut::<Assets<WorldAsset>>()
            .add(WorldAsset::new(world));
        let holders: Vec<_> = (0..3)
            .map(|_| app.world_mut().spawn(WorldAssetRoot(scene.clone())).id())
            .collect();
        for _ in 0..4 {
            app.update();
        }
        assert_eq!(app.world().resource::<Ready>().0, 3);
        let meshes = |app: &App, holder| {
            let children = app.world().get::<Children>(holder).unwrap();
            let root = children
                .iter()
                .find(|&entity| app.world().get::<expressions::Binds>(entity).is_some())
                .unwrap();
            let children = app.world().get::<Children>(root).unwrap();
            children
                .iter()
                .filter_map(|entity| {
                    app.world()
                        .get::<MeshMaterial3d<StandardMaterial>>(entity)
                        .map(|m| m.0.clone())
                })
                .collect::<Vec<_>>()
        };
        for &holder in &holders {
            assert_eq!(
                meshes(&app, holder),
                [original.clone(), original.clone(), missing.clone()]
            );
        }
        app.world_mut().entity_mut(holders[2]).despawn();
        app.world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .insert(missing.id(), StandardMaterial::default())
            .unwrap();
        for _ in 0..4 {
            app.update();
        }
        let a = meshes(&app, holders[0]);
        let b = meshes(&app, holders[1]);
        for handles in [&a, &b] {
            assert_eq!(handles[0], handles[1]);
            assert_ne!(handles[0], handles[2]);
            assert_ne!(handles[0], original);
            assert_ne!(handles[2], missing);
            let material = app
                .world()
                .resource::<Assets<StandardMaterial>>()
                .get(&handles[0])
                .unwrap();
            assert!(
                material
                    .uv_transform
                    .matrix2
                    .abs_diff_eq(uv.affine().matrix2, 1e-6)
            );
            assert!(
                material
                    .uv_transform
                    .translation
                    .abs_diff_eq(uv.offset, 1e-6)
            );
        }
        assert_ne!(a[0], b[0]);
        assert_ne!(a[2], b[2]);
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(meshes(&app, holders[0]), a);
        assert_eq!(meshes(&app, holders[1]), b);

        let root = app.world_mut().spawn(Transform::default()).id();
        expressions::attach(
            app.world_mut(),
            root,
            Some(&definitions),
            &HashMap::default(),
        )
        .unwrap();
        let mesh = app
            .world_mut()
            .spawn((
                MeshMaterial3d(original.clone()),
                MaterialSlot(0),
                ChildOf(root),
            ))
            .id();
        app.update();
        assert_ne!(
            app.world()
                .get::<MeshMaterial3d<StandardMaterial>>(mesh)
                .unwrap()
                .0,
            original
        );
    }

    #[test]
    fn review_pending_materials_follow_live_handles_slots_and_children() {
        use crate::material::MaterialSlot;
        use bevy::platform::collections::HashMap;
        let mut app = app();
        let (missing, available) = {
            let mut assets = app.world_mut().resource_mut::<Assets<StandardMaterial>>();
            (
                assets.reserve_handle(),
                assets.add(StandardMaterial::default()),
            )
        };
        let definitions = serde_json::from_value(json!({"custom":{"tint":{
            "materialColorBinds":[{"material":0,"type":"color","targetValue":[1,0,0,1]}]
        }}}))
        .unwrap();
        for change in ["handle", "slot", "child"] {
            let root = app.world_mut().spawn(Transform::default()).id();
            expressions::attach(
                app.world_mut(),
                root,
                Some(&definitions),
                &HashMap::default(),
            )
            .unwrap();
            let mesh = app
                .world_mut()
                .spawn((
                    MaterialSlot(0),
                    MeshMaterial3d(missing.clone()),
                    ChildOf(root),
                ))
                .id();
            app.update();
            assert_eq!(
                app.world()
                    .get::<MeshMaterial3d<StandardMaterial>>(mesh)
                    .unwrap()
                    .0,
                missing
            );
            let live = match change {
                "handle" => {
                    app.world_mut()
                        .entity_mut(mesh)
                        .insert(MeshMaterial3d(available.clone()));
                    mesh
                }
                "slot" => {
                    app.world_mut().entity_mut(mesh).insert(MaterialSlot(1));
                    app.world_mut()
                        .spawn((
                            MaterialSlot(0),
                            MeshMaterial3d(available.clone()),
                            ChildOf(root),
                        ))
                        .id()
                }
                _ => {
                    app.world_mut().entity_mut(mesh).despawn();
                    app.world_mut()
                        .spawn((
                            MaterialSlot(0),
                            MeshMaterial3d(available.clone()),
                            ChildOf(root),
                        ))
                        .id()
                }
            };
            app.update();
            let cloned = app
                .world()
                .get::<MeshMaterial3d<StandardMaterial>>(live)
                .unwrap()
                .0
                .clone();
            assert_ne!(
                cloned, available,
                "{change} must initialize without the old asset arriving"
            );
            assert_ne!(cloned, missing);
            let target = app
                .world()
                .get::<expressions::Binds>(root)
                .unwrap()
                .target(&expressions::ExpressionName::Custom("tint".into()))
                .unwrap();
            app.add_systems(
                PostUpdate,
                (move |mut inputs: Query<&mut animation::ExpressionInput>| {
                    inputs.get_mut(target).unwrap().value = Some(1.);
                })
                .in_set(VrmSystems::LookAt),
            );
            app.update();
            assert_eq!(
                app.world()
                    .resource::<Assets<StandardMaterial>>()
                    .get(&cloned)
                    .unwrap()
                    .base_color,
                Color::linear_rgba(1., 0., 0., 1.)
            );
            if change == "slot" {
                assert_eq!(
                    app.world()
                        .get::<MeshMaterial3d<StandardMaterial>>(mesh)
                        .unwrap()
                        .0,
                    missing
                );
            }
            for _ in 0..3 {
                app.update();
            }
            assert_eq!(
                app.world()
                    .get::<MeshMaterial3d<StandardMaterial>>(live)
                    .unwrap()
                    .0,
                cloned
            );
        }
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
                assert!(
                    chain.joints[0]
                        .motion
                        .unwrap()
                        .iter()
                        .all(|p| p.is_finite())
                );
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
        assert!(rig.chains[0].joints[0].motion.is_some());
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
        assert!(
            (0..10).any(|_| {
                app.update();
                !app.world()
                    .get::<Transform>(head_entity)
                    .unwrap()
                    .rotation
                    .abs_diff_eq(simulated, 1e-5)
            }),
            "resumed spring did not move"
        );
    }
}
