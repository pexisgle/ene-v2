use super::*;
use bevy::{
    animation::{AnimatedBy, AnimationTargetId},
    asset::{LoadState, RenderAssetUsages},
    gltf::Gltf,
    light::NotShadowCaster,
    mesh::morph::MorphWeights,
    platform::collections::HashMap,
    render::render_resource::{Extent3d, ShaderType, TextureDimension, TextureFormat},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct Files(PathBuf);
impl Files {
    pub(super) fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "ene-vrm-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    pub(super) fn write(&self, name: &str, value: &Value) -> String {
        let path = self.0.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    }
    pub(super) fn model(&self) -> Value {
        json!({
            "asset":{"version":"2.0"},"scene":0,"scenes":[{"nodes":[0,1]}],
            "nodes":[
                {"name":"Same"},
                {"name":"Same","translation":[0,1,0],"children":[2]},
                {"name":"Same","translation":[0,1,0]}
            ],
            "extensions":{"VRMC_vrm":{
                "specVersion":"1.0",
                "humanoid":{"humanBones":{"hips":{"node":1},"spine":{"node":2}}},
                "firstPerson":{"meshAnnotations":[{"node":0,"type":"both"}]},
                "expressions":{"preset":{"aa":{},"happy":{"overrideMouth":"block"}}}
            }}
        })
    }
    fn motion(&self, interpolation: &str) -> Value {
        let values: [f32; 16] = [
            0., 1., 0., 1., 0., 1., 1., 0., 0., 0., 0., 1., 0.19866933, 0., 0., 0.9800666,
        ];
        std::fs::write(
            self.0.join("motion.bin"),
            values
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let mut v = self.model();
        v["extensions"] = json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":1},"spine":{"node":2}}}}});
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
impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) fn app() -> App {
    app_with_vrm(true)
}

#[derive(Resource, Default)]
struct BeforeAnimation(bevy::ecs::change_detection::Tick);

fn app_with_vrm(vrm: bool) -> App {
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
    .add_plugins(bevy::render::sync_world::SyncWorldPlugin)
    .register_asset_loader(bevy::image::ImageLoader::new(
        bevy::image::CompressedImageFormats::NONE,
    ))
    .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f64(1. / 60.),
    ));
    if vrm {
        app.add_plugins(VrmPlugin)
            .init_resource::<BeforeAnimation>()
            .add_systems(
                PostUpdate,
                (|mut b: ResMut<BeforeAnimation>, ticks: bevy::ecs::system::SystemChangeTick| {
                    b.0 = ticks.this_run();
                })
                .before(bevy::app::AnimationSystems),
            );
    }
    app.finish();
    app.cleanup();
    app
}

pub(super) fn settle<A: Asset>(app: &mut App, handles: &[Handle<A>]) {
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
fn loader_validates_vrm_and_vrma_specifications() {
    let files = Files::new();
    let mut native = app_with_vrm(false);
    let mut invalid = files.model();
    invalid["extensions"]["VRMC_vrm"] = json!({"specVersion":"0.0"});
    let path = files.write("invalid-native.vrm", &invalid);
    let handles: Vec<Handle<Gltf>> = [
        files.write("valid-native.vrm", &files.model()),
        path.clone(),
    ]
    .into_iter()
    .map(|p| native.world().resource::<AssetServer>().load(p))
    .collect();
    settle(&mut native, &handles);
    for h in handles {
        assert!(
            native
                .world()
                .resource::<AssetServer>()
                .is_loaded_with_dependencies(h.id())
        );
    }
    let clip: Handle<AnimationClip> = native.world().resource::<AssetServer>().load(format!(
        "{}#Animation0",
        files.write("native-subasset.vrma", &files.motion("LINEAR"))
    ));
    settle(&mut native, std::slice::from_ref(&clip));
    assert!(matches!(
        native
            .world()
            .resource::<AssetServer>()
            .get_load_state(clip.id()),
        Some(LoadState::Failed(_))
    ));

    let mut app = app();
    let model = files.model();
    let motion = files.motion("LINEAR");
    let mut cases = Vec::new();
    for anim in [false, true] {
        let base = if anim { &motion } else { &model };
        let ext = if anim {
            "VRMC_vrm_animation"
        } else {
            "VRMC_vrm"
        };
        for m in 0..if anim { 11 } else { 9 } {
            let mut v = base.clone();
            match m {
                0 => v["extensions"] = json!({}),
                1 => v["extensions"][ext]["humanoid"]["humanBones"] = json!({}),
                2 => v["extensions"][ext]["humanoid"]["humanBones"]["hips"]["node"] = json!(99),
                3 => v["scenes"][0]["nodes"] = json!([]),
                4 => v["scenes"] = json!([]),
                5 => v["extensions"][ext]["specVersion"] = json!("0.0"),
                // Hierarchy violation: spine is node 0 (not descendant of hips node 1)
                6 => v["extensions"][ext]["humanoid"]["humanBones"]["spine"]["node"] = json!(0),
                // Unknown humanoid bone name
                7 => {
                    v["extensions"][ext]["humanoid"]["humanBones"]["unknownBone"] =
                        json!({"node":2})
                }
                8 if !anim => {
                    v["extensions"][ext]["firstPerson"]["meshAnnotations"] = json!([{"node":99}])
                }
                8 => v["animations"] = json!([]),
                9 => v["accessors"][0]["count"] = json!(0),
                // Prohibited eye bone or neutral/gaze preset in VRMA
                _ => v["extensions"][ext]["expressions"]["preset"]["lookUp"] = json!({"node":0}),
            }
            cases.push((anim, v));
        }
    }
    let mut vrma_eye = motion.clone();
    vrma_eye["nodes"][2]["children"] = json!([3, 4]);
    vrma_eye["nodes"]
        .as_array_mut()
        .unwrap()
        .extend([json!({}), json!({})]);
    vrma_eye["extensions"]["VRMC_vrm_animation"]["humanoid"]["humanBones"]["head"] =
        json!({"node":3});
    vrma_eye["extensions"]["VRMC_vrm_animation"]["humanoid"]["humanBones"]["leftEye"] =
        json!({"node":4});
    cases.push((true, vrma_eye));

    for spring in [
        json!({}),
        json!({"specVersion":"1.0","springs":"invalid"}),
        json!({"specVersion":"1.0","springs":[{"joints":[]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1},{"node":1}]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1},{"node":2,"hitRadius":-1.}]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":99}]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1}],"colliderGroups":[0]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1}],"center":99}]}),
        json!({"specVersion":"1.0","colliders":[{"node":1,"shape":{"sphere":{"radius":-1.}}}]}),
        json!({"specVersion":"1.0","colliderGroups":[{"colliders":[0]}]}),
    ] {
        let mut v = model.clone();
        v["extensions"]["VRMC_springBone"] = spring;
        cases.push((false, v));
    }
    for constraint in [
        json!({"rotation":{"source":99}}),
        json!({"rotation":{"source":1,"weight":2.}}),
        json!({"rotation":{"source":0}}),
    ] {
        let mut v = model.clone();
        v["nodes"][0]["extensions"]["VRMC_node_constraint"] =
            json!({"specVersion":"1.0","constraint":constraint});
        cases.push((false, v));
    }
    for mtoon in [
        json!({"specVersion":"1.0","rimLightingMixFactor":"invalid"}),
        json!({"specVersion":"1.0","renderQueueOffsetNumber":10.0}),
        json!({"specVersion":"1.0","shadeMultiplyTexture":{"index":0}}),
    ] {
        let mut v = model.clone();
        v["materials"] = json!([{"extensions":{"VRMC_materials_mtoon":mtoon}}]);
        cases.push((false, v));
    }
    let mut bad_thumb = model.clone();
    bad_thumb["extensions"]["VRMC_vrm"]["meta"] = json!({"thumbnailImage":0});
    cases.push((false, bad_thumb));

    let handles: Vec<Handle<Gltf>> = cases
        .into_iter()
        .enumerate()
        .map(|(i, (anim, v))| {
            app.world()
                .resource::<AssetServer>()
                .load_builder()
                .with_settings(|s: &mut bevy::gltf::GltfLoaderSettings| {
                    s.load_meshes = RenderAssetUsages::empty();
                    s.load_materials = RenderAssetUsages::empty();
                })
                .load(files.write(
                    &format!("case-{i}.{}", if anim { "vrma" } else { "vrm" }),
                    &v,
                ))
        })
        .collect();
    settle(&mut app, &handles);
    for h in &handles {
        assert!(
            matches!(
                app.world().resource::<AssetServer>().get_load_state(h.id()),
                Some(LoadState::Failed(_))
            ),
            "accepted invalid {:?}",
            h.path()
        );
    }
}

#[test]
fn vrma_curves_support_linear_step_cubic_and_scene_conversion() {
    let files = Files::new();
    let mut app = app();
    let mut motion = files.motion("LINEAR");
    motion["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({"node":0});
    motion["animations"][0]["channels"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            json!({"sampler":1,"target":{"node":0,"path":"rotation"}}),
        );

    // Same-node multi-channel with ignored scale channel
    let mut same_node = files.motion("LINEAR");
    let mut bytes = std::fs::read(files.0.join("motion.bin")).unwrap();
    bytes.extend(
        [2f32, 2., 2., 3., 3., 3.]
            .into_iter()
            .flat_map(f32::to_le_bytes),
    );
    std::fs::write(files.0.join("same.bin"), bytes).unwrap();
    same_node["buffers"] = json!([{"byteLength":88,"uri":"same.bin"}]);
    same_node["bufferViews"]
        .as_array_mut()
        .unwrap()
        .push(json!({"buffer":0,"byteOffset":64,"byteLength":24}));
    same_node["accessors"]
        .as_array_mut()
        .unwrap()
        .push(json!({"bufferView":3,"componentType":5126,"count":2,"type":"VEC3"}));
    same_node["animations"][0]["samplers"]
        .as_array_mut()
        .unwrap()
        .push(json!({"input":0,"output":3,"interpolation":"LINEAR"}));
    same_node["animations"][0]["channels"] = json!([
        {"sampler":1,"target":{"node":1,"path":"rotation"}},
        {"sampler":2,"target":{"node":1,"path":"scale"}},
        {"sampler":0,"target":{"node":1,"path":"translation"}}
    ]);

    let step = files.motion("STEP");
    let mut cubic = files.motion("LINEAR");
    let cubic_vals: [f32; 20] = [
        0., 1., 0., 0., 0., 0., 1., 0., 2., 0., 0., 2., 0., 0., 1., 1., 0., 0., 0., 0.,
    ];
    std::fs::write(
        files.0.join("cubic.bin"),
        cubic_vals
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    cubic["buffers"] = json!([{"byteLength":80,"uri":"cubic.bin"}]);
    cubic["bufferViews"] =
        json!([{"buffer":0,"byteLength":8},{"buffer":0,"byteOffset":8,"byteLength":72}]);
    cubic["accessors"].as_array_mut().unwrap().truncate(2);
    cubic["accessors"][1]["count"] = json!(6);
    cubic["animations"] = json!([{"samplers":[{"input":0,"output":1,"interpolation":"CUBICSPLINE"}],"channels":[{"sampler":0,"target":{"node":1,"path":"translation"}}]}]);

    let h_rot: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load_builder()
        .with_settings(|s: &mut bevy::gltf::GltfLoaderSettings| {
            s.include_source = true;
            s.convert_coordinates = Some(bevy::gltf::convert_coordinates::GltfConvertCoordinates {
                rotate_scene_entity: true,
                rotate_meshes: false,
            });
        })
        .load(files.write("rot.vrma", &motion));
    let h_same: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("same.vrma", &same_node));
    let h_step: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("step.vrma", &step));
    let h_cubic: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("cubic.vrma", &cubic));
    settle(
        &mut app,
        &[
            h_rot.clone(),
            h_same.clone(),
            h_step.clone(),
            h_cubic.clone(),
        ],
    );

    let assets = app.world().resource::<Assets<Gltf>>();
    let clips = app.world().resource::<Assets<AnimationClip>>();
    assert!(assets.get(&h_rot).unwrap().source.is_some());
    let rot_clip = clips
        .get(&assets.get(&h_rot).unwrap().animations[0])
        .unwrap();
    assert!(
        rot_clip
            .curves_for_target(animation::bone_target("hips"))
            .unwrap()[0]
            .0
            .sample_clamped(0.5)
            .downcast::<Vec3>()
            .unwrap()
            .abs_diff_eq(Vec3::X * 0.5, 1e-6)
    );
    assert!(
        rot_clip
            .curves_for_target(animation::gaze_target())
            .unwrap()[0]
            .0
            .sample_clamped(0.5)
            .downcast::<Quat>()
            .unwrap()
            .abs_diff_eq(Quat::from_rotation_x(0.2), 1e-6)
    );

    let same_curves = clips
        .get(&assets.get(&h_same).unwrap().animations[0])
        .unwrap()
        .curves_for_target(animation::bone_target("hips"))
        .unwrap();
    assert_eq!(same_curves.len(), 2);

    let step_clip = clips
        .get(&assets.get(&h_step).unwrap().animations[0])
        .unwrap();
    assert_eq!(
        *step_clip
            .curves_for_target(animation::bone_target("hips"))
            .unwrap()[0]
            .0
            .sample_clamped(0.999)
            .downcast::<Vec3>()
            .unwrap(),
        Vec3::ZERO
    );
    assert_eq!(
        *step_clip
            .curves_for_target(animation::bone_target("hips"))
            .unwrap()[0]
            .0
            .sample_clamped(1.)
            .downcast::<Vec3>()
            .unwrap(),
        Vec3::X
    );

    let cubic_clip = clips
        .get(&assets.get(&h_cubic).unwrap().animations[0])
        .unwrap();
    assert!(
        cubic_clip
            .curves_for_target(animation::bone_target("hips"))
            .unwrap()[0]
            .0
            .sample_clamped(0.25)
            .downcast::<Vec3>()
            .unwrap()
            .abs_diff_eq(Vec3::X * 0.34375, 1e-6)
    );
}

#[test]
fn scene_instances_remap_bones_and_preserve_native_and_vrma_playback() {
    let files = Files::new();
    let mut app = app();
    let mut model = files.model();
    model["scenes"] = json!([{"nodes":[0]},{"nodes":[0,1]}]);
    model["scene"] = json!(1);
    model["nodes"][2]["children"] = json!([3]);
    model["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name":"Head","translation":[0.,1.,0.],"children":[4,5]}));
    model["nodes"].as_array_mut().unwrap().extend([
        json!({"name":"LeftEye","translation":[0.1,0.1,0.],"rotation":Quat::from_rotation_z(0.15).to_array()}),
        json!({"name":"RightEye","translation":[-0.1,0.1,0.]})
    ]);
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["head"] = json!({"node":3});
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["leftEye"] = json!({"node":4});
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["rightEye"] = json!({"node":5});
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
                v.bone("hips").unwrap(),
                v.bone("spine").unwrap(),
                v.bone("head").unwrap(),
                v.bone("leftEye").unwrap(),
            )
        })
        .collect();
    assert_eq!(rigs.len(), 2);

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

    // LookAt target and overrideLookAt suppression on first instance
    let (root, _, _, head, eye) = rigs[0];
    let eye_rest = app
        .world()
        .get::<animation::RestPose>(eye)
        .unwrap()
        .local
        .rotation;
    app.world_mut()
        .get_mut::<AnimationPlayer>(root)
        .unwrap()
        .stop_all();
    app.update();
    assert!(!gaze_input_active(&app, root));
    assert!(
        app.world()
            .get::<Transform>(eye)
            .unwrap()
            .rotation
            .abs_diff_eq(eye_rest, 1e-5)
    );

    let head_tf = *app.world().get::<GlobalTransform>(head).unwrap();
    let head_rest = app
        .world()
        .get::<animation::RestPose>(head)
        .unwrap()
        .global_rotation;
    let side_target = head_tf.transform_point(Vec3::new(0.02, 0.03, 0.04))
        + head_tf.rotation().normalize() * head_rest.inverse() * Vec3::new(1., 0., 1.);
    app.world_mut().get_mut::<LookAt>(root).unwrap().target = Some(side_target);
    app.world_mut()
        .get_mut::<ExpressionWeights>(root)
        .unwrap()
        .insert("blink".into(), 1.);
    app.update();
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
fn expression_morphs_materials_and_vrma_blending_stay_isolated() {
    let files = Files::new();
    let mut app = app();
    let mut value = files.motion("STEP");
    value["extensions"] = json!({"VRMC_vrm_animation":{"specVersion":"1.0","expressions":{"preset":{"happy":{"node":1}}}}});
    value["animations"][0]["channels"] =
        json!([{"sampler":0,"target":{"node":1,"path":"translation"}}]);
    let handle: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load(files.write("expr.vrma", &value));
    settle(&mut app, std::slice::from_ref(&handle));
    let clip = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&handle)
        .unwrap()
        .animations[0]
        .clone();

    let mut graph = AnimationGraph::new();
    let a = graph.add_clip(clip.clone(), 0.25, graph.root);
    let b = graph.add_clip(clip, 0.75, graph.root);
    let graph = app
        .world_mut()
        .resource_mut::<Assets<AnimationGraph>>()
        .add(graph);

    let definitions: schema::Expressions = serde_json::from_value(json!({"preset":{
        "aa":{"morphTargetBinds":[{"node":0,"index":0,"weight":1.}]},
        "happy":{
            "overrideMouth":"block",
            "materialColorBinds":[{"material":0,"type":"color","targetValue":[1.,0.,0.,1.]}],
            "textureTransformBinds":[{"material":0,"scale":[2.,1.],"offset":[0.5,0.]}]
        }
    }}))
    .unwrap();
    let orig_std = app
        .world_mut()
        .resource_mut::<Assets<StandardMaterial>>()
        .add(StandardMaterial::default());
    let toon_def = serde_json::from_value(json!({"specVersion":"1.0"})).unwrap();
    let orig_toon = app
        .world_mut()
        .resource_mut::<Assets<MToonMaterial>>()
        .add(MToonMaterial {
            base: StandardMaterial::default(),
            extension: MToonExtension::new(&toon_def, &[]),
        });

    let mut rigs = Vec::new();
    for happy in [1., 0.] {
        let mut player = AnimationPlayer::default();
        player.play(a).seek_to(1.).pause();
        player.play(b).seek_to(0.).pause();
        let root = app
            .world_mut()
            .spawn((
                player,
                AnimationGraphHandle(graph.clone()),
                Transform::default(),
            ))
            .id();
        let morph = app
            .world_mut()
            .spawn((
                MorphWeights::new(vec![0., 0.7], None).unwrap(),
                ChildOf(root),
            ))
            .id();
        expressions::attach(
            app.world_mut(),
            root,
            &definitions,
            &HashMap::from_iter([(0, morph)]),
        )
        .unwrap();
        app.world_mut()
            .get_mut::<ExpressionWeights>(root)
            .unwrap()
            .extend([("aa".into(), 1.), ("happy".into(), happy)]);
        let std_mesh = app
            .world_mut()
            .spawn((
                MeshMaterial3d(orig_std.clone()),
                material::MaterialSlot(0),
                ChildOf(root),
            ))
            .id();
        let toon_mesh = app
            .world_mut()
            .spawn((
                MeshMaterial3d(orig_toon.clone()),
                material::MaterialSlot(0),
                ChildOf(root),
            ))
            .id();
        rigs.push((root, morph, std_mesh, toon_mesh));
    }
    for _ in 0..3 {
        app.update();
    }

    // Manual weights win over animation; mouth override blocks "aa" on rig 0
    assert_eq!(
        app.world()
            .get::<MorphWeights>(rigs[0].1)
            .unwrap()
            .weights(),
        &[0., 0.7]
    );
    assert_eq!(
        app.world()
            .get::<MorphWeights>(rigs[1].1)
            .unwrap()
            .weights(),
        &[1., 0.7]
    );
    let std_h0 = app
        .world()
        .get::<MeshMaterial3d<StandardMaterial>>(rigs[0].2)
        .unwrap()
        .0
        .clone();
    let toon_h0 = app
        .world()
        .get::<MeshMaterial3d<MToonMaterial>>(rigs[0].3)
        .unwrap()
        .0
        .clone();
    assert_ne!(std_h0, orig_std);
    assert_ne!(toon_h0, orig_toon);
    assert_eq!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&std_h0)
            .unwrap()
            .base_color
            .to_linear(),
        LinearRgba::RED
    );
    assert_eq!(
        app.world()
            .resource::<Assets<MToonMaterial>>()
            .get(&toon_h0)
            .unwrap()
            .base
            .base_color
            .to_linear(),
        LinearRgba::RED
    );

    // Removing manual key restores blended VRMA weight (0.25)
    app.world_mut()
        .get_mut::<ExpressionWeights>(rigs[0].0)
        .unwrap()
        .remove("happy");
    app.update();
    let ch = app
        .world()
        .get::<expressions::Binds>(rigs[0].0)
        .unwrap()
        .target("happy")
        .unwrap();
    assert_eq!(
        app.world().get::<Transform>(ch).unwrap().translation.x,
        0.25
    );

    // Stopping animation resets channel to zero
    app.world_mut()
        .get_mut::<AnimationPlayer>(rigs[0].0)
        .unwrap()
        .stop_all();
    app.update();
    assert_eq!(
        app.world().get::<Transform>(ch).unwrap().translation,
        Vec3::ZERO
    );
    assert_eq!(
        app.world()
            .get::<MorphWeights>(rigs[0].1)
            .unwrap()
            .weights(),
        &[1., 0.7]
    );
}

#[test]
fn mtoon_materials_outlines_meta_and_schema_defaults() {
    let toon: schema::Toon = serde_json::from_value(json!({"specVersion":"1.0"})).unwrap();
    assert_eq!(toon.params.shade_color_factor, Vec3::ONE);
    assert_eq!(toon.params.shading_toony_factor, 0.9);
    assert_eq!(toon.outline_width_mode, schema::OutlineMode::None);
    assert_eq!(schema::ToonParams::min_size().get(), 112);

    let gaze: schema::Gaze = serde_json::from_value(json!({"type":"expression"})).unwrap();
    assert_eq!(gaze.horizontal_inner().output_scale, 1.0);
    let motion: schema::Motion = serde_json::from_value(
        json!({"specVersion":"1.0","lookAt":{"offsetFromHeadBone":[0.,0.06,0.]}}),
    )
    .unwrap();
    assert_eq!(
        motion.look_at.unwrap().offset_from_head_bone,
        Vec3::new(0., 0.06, 0.)
    );

    let files = Files::new();
    let mut app = app();
    Image::new_fill(
        Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[128, 128, 128, 255],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    )
    .try_into_dynamic()
    .unwrap()
    .save(files.0.join("tex.png"))
    .unwrap();

    let vertices = [0f32, 0., 0., 1., 0., 0., 0., 1., 0.];
    let mut data: Vec<_> = vertices.into_iter().flat_map(f32::to_le_bytes).collect();
    data.extend([0u8; 12]);
    data.extend(
        [1f32, 0., 0., 0.]
            .repeat(3)
            .into_iter()
            .flat_map(f32::to_le_bytes),
    );
    std::fs::write(files.0.join("tri.bin"), data).unwrap();

    let mut value = files.model();
    value["buffers"] = json!([{"uri":"tri.bin","byteLength":96}]);
    value["bufferViews"] = json!([{"buffer":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":12},{"buffer":0,"byteOffset":48,"byteLength":48}]);
    value["accessors"] = json!([
        {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
        {"bufferView":1,"componentType":5121,"count":3,"type":"VEC4"},
        {"bufferView":2,"componentType":5126,"count":3,"type":"VEC4"}
    ]);
    value["images"] = json!([{"uri":"tex.png"}]);
    value["textures"] = json!([{"source":0}]);
    value["meshes"] = json!([{"primitives":[
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":0},
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":1},
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":2}
    ]}]);
    value["skins"] = json!([{"joints":[1]}]);
    value["nodes"][0]["skin"] = json!(0);
    value["nodes"][0]["mesh"] = json!(0);
    value["nodes"][1]["mesh"] = json!(0);
    value["nodes"][1]["scale"] = json!([-1, 1, 1]);
    value["materials"] = json!([
        {
            "alphaMode":"OPAQUE",
            "pbrMetallicRoughness":{"baseColorTexture":{"index":0,"extensions":{"KHR_texture_transform":{"offset":[0.5,0.25]}}}},
            "extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0","outlineWidthMode":"screenCoordinates","outlineWidthFactor":0.001,"renderQueueOffsetNumber":5.0,"shadingShiftTexture":{"index":0}}}
        },
        {
            "alphaMode":"BLEND",
            "extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0","renderQueueOffsetNumber":3.0}}
        },
        {
            "alphaMode":"BLEND",
            "extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0","transparentWithZWrite":true,"renderQueueOffsetNumber":2.0}}
        }
    ]);
    value["extensions"]["VRMC_vrm"]["meta"] = json!({
        "name":"Ene Avatar",
        "authors":["Test Author"],
        "thumbnailImage":0,
        "commercialUsage":"personalCommercial"
    });
    let path = files.write("full.vrm", &value);
    let handle: Handle<Gltf> = app.world().resource::<AssetServer>().load(path.clone());
    let m0: Handle<MToonMaterial> = app
        .world()
        .resource::<AssetServer>()
        .load(format!("{path}#Material0/vrm"));
    let m1: Handle<MToonMaterial> = app
        .world()
        .resource::<AssetServer>()
        .load(format!("{path}#Material1/vrm"));
    let m2: Handle<MToonMaterial> = app
        .world()
        .resource::<AssetServer>()
        .load(format!("{path}#Material2/vrm"));
    settle(&mut app, std::slice::from_ref(&handle));

    let materials = app.world().resource::<Assets<MToonMaterial>>();
    let mat0 = materials.get(&m0).unwrap();
    assert_eq!(mat0.base.depth_bias, 0.0);
    assert_eq!(mat0.base.uv_transform.translation, Vec2::new(0.5, 0.25));
    assert!(mat0.extension.shift.is_some());
    assert_eq!(materials.get(&m1).unwrap().base.depth_bias, 3.0);
    assert_eq!(materials.get(&m2).unwrap().base.depth_bias, 2.0 - 10.0);

    let scene = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&handle)
        .unwrap()
        .default_scene
        .clone()
        .unwrap();
    app.world_mut().spawn(WorldAssetRoot(scene));
    for _ in 0..5 {
        app.update();
    }
    let vrm = app.world_mut().query::<&Vrm>().single(app.world()).unwrap();
    assert_eq!(vrm.meta.as_ref().unwrap().name, "Ene Avatar");
    assert!(vrm.thumbnail.is_some());
    let outlines = app
        .world_mut()
        .query_filtered::<&MeshMaterial3d<MToonMaterial>, With<NotShadowCaster>>()
        .iter(app.world())
        .count();
    assert_eq!(outlines, 2);
}

#[test]
#[ignore = "requires a GPU and ENE_VRM_TEST_MODEL pointing to a licensed VRM fixture"]
fn gpu_model_material_skin_morph_and_outline_smoke() {
    use bevy::render::{
        RenderApp, RenderPlugin,
        render_resource::{CachedPipelineState, PipelineCache},
    };
    let path = std::env::var("ENE_VRM_TEST_MODEL").expect("set ENE_VRM_TEST_MODEL");
    let plugins = DefaultPlugins
        .build()
        .disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>();
    #[cfg(windows)]
    let plugins = plugins.disable::<bevy::winit::WinitPlugin>();
    let mut app = App::new();
    app.add_plugins(
        plugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: bevy::window::ExitCondition::DontExit,
                ..default()
            })
            .set(AssetPlugin {
                unapproved_path_mode: bevy::asset::UnapprovedPathMode::Allow,
                ..default()
            })
            .set(RenderPlugin {
                synchronous_pipeline_compilation: true,
                #[cfg(windows)]
                render_creation: bevy::render::settings::RenderCreation::Automatic(Box::new(
                    bevy::render::settings::WgpuSettings {
                        backends: Some(
                            bevy::render::settings::Backends::from_env()
                                .unwrap_or(bevy::render::settings::Backends::DX12),
                        ),
                        ..default()
                    },
                )),
                ..default()
            }),
    )
    .add_plugins(VrmPlugin)
    .insert_resource(ClearColor(Color::NONE));
    while app.plugins_state() != bevy::app::PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
        std::thread::yield_now();
    }
    app.finish();
    app.cleanup();
    let model: Handle<Gltf> = app.world().resource::<AssetServer>().load(path);
    settle(&mut app, std::slice::from_ref(&model));
    let scene = app
        .world()
        .resource::<Assets<Gltf>>()
        .get(&model)
        .expect("model failed to load")
        .default_scene
        .clone()
        .unwrap();
    for x in [-0.4, 0.4] {
        app.world_mut().spawn((
            WorldAssetRoot(scene.clone()),
            Transform::from_xyz(x, 0., 0.),
        ));
    }
    let image = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new_target_texture(
            512,
            512,
            TextureFormat::Rgba8UnormSrgb,
            None,
        ));
    app.world_mut().spawn((
        Camera3d::default(),
        bevy::core_pipeline::tonemapping::Tonemapping::None,
        bevy::camera::RenderTarget::Image(image.clone().into()),
        Transform::from_xyz(0., 1., 3.).looking_at(Vec3::new(0., 0.9, 0.), Vec3::Y),
    ));
    app.world_mut().spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.4, -0.3, 0.)),
    ));
    for _ in 0..60 {
        app.update();
        assert!(app.should_exit().is_none());
    }
    for pipeline in app
        .sub_app(RenderApp)
        .world()
        .resource::<PipelineCache>()
        .pipelines()
    {
        assert!(
            !matches!(pipeline.state, CachedPipelineState::Err(_)),
            "{:?}",
            pipeline.state
        );
    }
    assert!(
        app.world_mut()
            .query::<&MeshMaterial3d<MToonMaterial>>()
            .iter(app.world())
            .count()
            > 0
    );
}
