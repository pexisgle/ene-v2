use super::*;
use crate::material::{BaseUv, EmissiveStrength, MaterialSlot, Outline, TextureSpace};
use bevy::{
    animation::{AnimatedBy, AnimationTargetId},
    asset::{LoadState, RenderAssetUsages},
    camera::visibility::{NoFrustumCulling, RenderLayers},
    gltf::Gltf,
    mesh::{
        VertexAttributeValues,
        morph::{MeshMorphWeights, MorphAttributes, MorphWeights},
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
    },
    platform::collections::HashMap,
    render::render_resource::{Extent3d, ShaderType, TextureDimension, TextureFormat},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) struct Files(tempfile::TempDir);
impl Files {
    pub(super) fn new() -> Self {
        Self(tempfile::tempdir().expect("create test fixture directory"))
    }
    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }
    pub(super) fn write(&self, name: &str, value: &Value) -> String {
        let path = self.join(name);
        std::fs::write(&path, value.to_string()).unwrap();
        path.to_str().unwrap().to_owned()
    }
    pub(super) fn model(&self) -> Value {
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
        v["extensions"] = json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":humanoid}});
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

pub(super) fn app() -> App {
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

/// Asserts that each main-world mesh entity was specialized and its pipeline
/// reached the GPU, instead of only checking that no pipeline failed.
fn assert_specialized_pipelines_ok(app: &App, entities: &[Entity]) {
    use bevy::pbr::SpecializedMaterialPipelineCache;
    use bevy::render::{
        RenderApp,
        render_resource::{CachedPipelineState, PipelineCache},
        sync_world::MainEntity,
    };
    let world = app.sub_app(RenderApp).world();
    let cache = world.resource::<SpecializedMaterialPipelineCache>();
    let pipelines = world.resource::<PipelineCache>();
    for entity in entities {
        let id = cache
            .values()
            .find_map(|view| view.get(&MainEntity::from(*entity)))
            .copied()
            .unwrap_or_else(|| panic!("{entity:?} has no specialized material pipeline"));
        assert!(
            matches!(
                pipelines.get_render_pipeline_state(id),
                CachedPipelineState::Ok(_)
            ),
            "{entity:?} pipeline was not created"
        );
    }
}

/// Headless app that actually renders: the render plugin stays on, no window is
/// created, pipelines compile synchronously, and time advances by 1/60 s per
/// update so the synthetic GPU tests are deterministic.
fn gpu_app(clear: ClearColor) -> App {
    use bevy::render::RenderPlugin;
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
            .set(RenderPlugin {
                synchronous_pipeline_compilation: true,
                ..default()
            }),
    )
    .add_plugins(VrmPlugin)
    .insert_resource(clear)
    .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
        Duration::from_secs_f64(1. / 60.),
    ));
    while app.plugins_state() != bevy::app::PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
        std::thread::yield_now();
    }
    app.finish();
    app.cleanup();
    app
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
fn loader_rejects_unusable_data_and_tolerates_spec_noise() {
    let files = Files::new();
    let mut app = app();
    let model = files.model();
    let motion = files.motion("LINEAR");

    let mut cases: Vec<(bool, Value, bool)> = Vec::new();
    let mut push = |v: Value, anim: bool, reject: bool| cases.push((anim, v, reject));

    // Missing or unreadable extension data, missing scenes and dangling indices
    // are structural problems the loader still has to report.
    let mut v = model.clone();
    v["extensions"] = json!({});
    push(v, false, true);
    let mut v = model.clone();
    v["extensions"]["VRMC_vrm"]["specVersion"] = json!("0.0");
    push(v, false, true);
    let mut v = model.clone();
    v["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["hips"]["node"] = json!(99);
    push(v, false, true);
    let mut v = model.clone();
    v["scenes"] = json!([]);
    push(v, false, true);
    let mut v = model.clone();
    v["extensions"]["VRMC_vrm"]
        .as_object_mut()
        .unwrap()
        .remove("meta");
    push(v, false, true);
    let mut v = motion.clone();
    v["animations"] = json!([]);
    push(v, true, true);
    let mut v = motion.clone();
    v["accessors"][0]["count"] = json!(0);
    push(v, true, true);
    let mut v = motion.clone();
    v["extensions"]["VRMC_vrm_animation"]["lookAt"] = json!({});
    push(v, true, true);

    // Physics and constraints still need valid indices and acyclic ordering.
    for spring in [
        json!({}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":99}]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1}],"colliderGroups":[0]}]}),
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1},{"node":2,"hitRadius":-1.}]}]}),
        json!({"specVersion":"1.0","colliders":[{"node":1,"shape":{"sphere":{"radius":-1.}}}]}),
    ] {
        let mut v = model.clone();
        v["extensions"]["VRMC_springBone"] = spring;
        push(v, false, true);
    }
    let mut v = model.clone();
    v["nodes"][0]["extensions"]["VRMC_node_constraint"] =
        json!({"specVersion":"1.0","constraint":{"rotation":{"source":0}}});
    push(v, false, true);
    let mut v = model.clone();
    v["nodes"][2]["extensions"]["VRMC_node_constraint"] = json!({
        "specVersion":"0.0",
        "constraint":{"rotation":{"source":1}}
    });
    push(v, false, true);
    let mut v = model.clone();
    v["nodes"][2]["extensions"]["VRMC_node_constraint"] = json!({
        "specVersion":"1.0",
        "constraint":{
            "rotation":{"source":1},
            "roll":{"source":1,"rollAxis":"X"}
        }
    });
    push(v, false, true);

    // VRM conformance is enforced by authoring tools, not by a renderer: these
    // documents are renderable and must not fail the load.
    let mut v = model.clone();
    v["extensions"]["VRMC_vrm"]["meta"]["name"] = json!("");
    v["extensions"]["VRMC_vrm"]["meta"]["authors"] = json!([]);
    v["extensions"]["VRMC_vrm"]["meta"]["references"] = json!([]);
    v["extensions"]["VRMC_vrm"]["meta"]["licenseUrl"] = json!("https://example.com/license");
    v["extensions"]["VRMC_vrm"]["meta"]["thumbnailImage"] = json!(0);
    push(v, false, false);
    let mut v = model.clone();
    v["extensions"]["VRMC_vrm"]["expressions"]["custom"] = json!({"aa":{},"unknown":{}});
    v["extensions"]["VRMC_vrm"]["expressions"]["preset"]["aa"] = json!({"morphTargetBinds":[]});
    push(v, false, false);
    let mut v = model.clone();
    v["nodes"][1]["scale"] = json!([1., 2., 1.]);
    let extra = v["nodes"].as_array().unwrap().len();
    v["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"scale":[0., 0., 0.]}));
    v["scenes"][0]["nodes"] = json!([0, extra]);
    push(v, false, false);
    let mut v = model.clone();
    v["materials"] = json!([
        {"alphaMode":"OPAQUE","extensions":{"VRMC_materials_mtoon":{
            "specVersion":"1.0","renderQueueOffsetNumber":10,
            "giEqualizationFactor":0.5,
            "shadingToonyFactor":2.0,"outlineWidthFactor":-1.0,
            "shadeMultiplyTexture":{"index":0}
        }}},
        {"alphaMode":"BLEND","extensions":{"VRMC_materials_mtoon":{
            "specVersion":"1.0","transparentWithZWrite":true,"renderQueueOffsetNumber":2
        }}}
    ]);
    push(v, false, false);
    let mut v = model.clone();
    v["extensions"]["VRMC_springBone"] = json!({
        "specVersion":"1.0",
        "springs":[
            {"joints":[{"node":1},{"node":3}],"center":4},
            {"joints":[{"node":2},{"node":10}]}
        ]
    });
    push(v, false, false);
    // glTF `extras`/`extensions` are valid inside constraint and collider shape
    // objects and must not be mistaken for a second variant.
    let mut v = model.clone();
    v["nodes"][2]["extensions"]["VRMC_node_constraint"] = json!({
        "specVersion":"1.0",
        "constraint":{
            "rotation":{"source":1},
            "extras":{"note":"authoring metadata"},
            "extensions":{}
        }
    });
    push(v, false, false);
    let mut v = model.clone();
    v["extensions"]["VRMC_springBone"] = json!({
        "specVersion":"1.0",
        "colliders":[{
            "node":1,
            "shape":{
                "sphere":{"radius":0.1},
                "extras":{"note":"authoring metadata"},
                "extensions":{}
            },
            "extras":{"note":"collider metadata"}
        }],
        "springs":[{"joints":[{"node":1},{"node":2}]}]
    });
    push(v, false, false);
    let mut v = motion.clone();
    v["animations"][0]["channels"]
        .as_array_mut()
        .unwrap()
        .extend([
            json!({"sampler":0,"target":{"node":2,"path":"scale"}}),
            json!({"sampler":0,"target":{"node":2,"path":"translation"}}),
        ]);
    push(v, true, false);

    // gltf-json does not know the VRMC extensions, but this crate does. A
    // document may require them without failing Bevy's validation.
    let mut v = model.clone();
    v["extensionsUsed"] = json!([
        "VRMC_vrm",
        "VRMC_materials_mtoon",
        "VRMC_springBone",
        "VRMC_node_constraint"
    ]);
    v["extensionsRequired"] = v["extensionsUsed"].clone();
    v["materials"] = json!([{"extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0"}}}]);
    v["extensions"]["VRMC_springBone"] =
        json!({"specVersion":"1.0","springs":[{"joints":[{"node":1},{"node":2}]}]});
    v["nodes"][0]["extensions"] = json!({
        "VRMC_node_constraint": {
            "specVersion": "1.0",
            "constraint": {"rotation": {"source": 1}}
        }
    });
    push(v, false, false);
    let mut v = motion.clone();
    v["extensionsUsed"] = json!(["VRMC_vrm_animation"]);
    v["extensionsRequired"] = json!(["VRMC_vrm_animation"]);
    push(v, true, false);
    // Unknown required extensions still fail validation.
    let mut v = model.clone();
    v["extensionsUsed"] = json!(["VRMC_vrm", "EXT_unknown"]);
    v["extensionsRequired"] = json!(["EXT_unknown"]);
    push(v, false, true);

    let handles: Vec<(bool, Handle<Gltf>)> = cases
        .into_iter()
        .enumerate()
        .map(|(i, (anim, v, reject))| {
            let handle = app
                .world()
                .resource::<AssetServer>()
                .load_builder()
                .with_settings(|s: &mut bevy::gltf::GltfLoaderSettings| {
                    s.load_meshes = RenderAssetUsages::empty();
                    s.load_materials = RenderAssetUsages::empty();
                })
                .load(files.write(
                    &format!("case-{i}.{}", if anim { "vrma" } else { "vrm" }),
                    &v,
                ));
            (reject, handle)
        })
        .collect();
    settle(
        &mut app,
        &handles.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
    );
    for (i, (reject, h)) in handles.iter().enumerate() {
        let failed = matches!(
            app.world().resource::<AssetServer>().get_load_state(h.id()),
            Some(LoadState::Failed(_))
        );
        assert_eq!(
            failed,
            *reject,
            "case {i} was {}",
            if failed { "rejected" } else { "accepted" }
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

    // Same-node multi-channel
    let mut same_node = files.motion("LINEAR");
    let mut bytes = std::fs::read(files.join("motion.bin")).unwrap();
    bytes.extend(
        [2f32, 2., 2., 3., 3., 3.]
            .into_iter()
            .flat_map(f32::to_le_bytes),
    );
    std::fs::write(files.join("same.bin"), bytes).unwrap();
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
        {"sampler":0,"target":{"node":1,"path":"translation"}}
    ]);

    let step = files.motion("STEP");
    let mut cubic = files.motion("LINEAR");
    let cubic_vals: [f32; 20] = [
        0., 1., 0., 0., 0., 0., 1., 0., 2., 0., 0., 2., 0., 0., 1., 1., 0., 0., 0., 0.,
    ];
    std::fs::write(
        files.join("cubic.bin"),
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
            .curves_for_target(animation::bone_target(HumanBoneName::Hips))
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
        .curves_for_target(animation::bone_target(HumanBoneName::Hips))
        .unwrap();
    assert_eq!(same_curves.len(), 2);

    let step_clip = clips
        .get(&assets.get(&h_step).unwrap().animations[0])
        .unwrap();
    assert_eq!(
        *step_clip
            .curves_for_target(animation::bone_target(HumanBoneName::Hips))
            .unwrap()[0]
            .0
            .sample_clamped(0.999)
            .downcast::<Vec3>()
            .unwrap(),
        Vec3::ZERO
    );
    assert_eq!(
        *step_clip
            .curves_for_target(animation::bone_target(HumanBoneName::Hips))
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
            .curves_for_target(animation::bone_target(HumanBoneName::Hips))
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
    model["scenes"] = json!([{"nodes":[0]},{"nodes":[0]}]);
    model["scene"] = json!(1);
    let eye = model["nodes"].as_array().unwrap().len();
    model["nodes"][3]["children"] = json!([eye, eye + 1]);
    model["nodes"].as_array_mut().unwrap().extend([
        json!({"name":"LeftEye","translation":[0.1,0.1,0.],"rotation":Quat::from_rotation_z(0.15).to_array()}),
        json!({"name":"RightEye","translation":[-0.1,0.1,0.]})
    ]);
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["leftEye"] = json!({"node":eye});
    model["extensions"]["VRMC_vrm"]["humanoid"]["humanBones"]["rightEye"] = json!({"node":eye + 1});
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
        .insert(ExpressionName::Preset(ExpressionPresetName::Blink), 1.);
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
            "materialColorBinds":[
                {"material":0,"type":"color","targetValue":[1.,0.,0.,1.]},
                {"material":0,"type":"emissionColor","targetValue":[0.4,0.25,0.,1.]}
            ],
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
            extension: MToonExtension::new(&toon_def, &[], &Default::default()),
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
            Some(&definitions),
            &HashMap::from_iter([(0, morph)]),
        )
        .unwrap();
        app.world_mut()
            .get_mut::<ExpressionWeights>(root)
            .unwrap()
            .extend([
                (ExpressionName::Preset(ExpressionPresetName::Aa), 1.),
                (ExpressionName::Preset(ExpressionPresetName::Happy), happy),
            ]);
        let std_mesh = app
            .world_mut()
            .spawn((
                MeshMaterial3d(orig_std.clone()),
                material::MaterialSlot(0),
                EmissiveStrength(10.),
                ChildOf(root),
            ))
            .id();
        let toon_mesh = app
            .world_mut()
            .spawn((
                MeshMaterial3d(orig_toon.clone()),
                material::MaterialSlot(0),
                EmissiveStrength(10.),
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
    // The target is a raw `emissiveFactor`, so the authored strength scales it
    // once; the already scaled rest color is not multiplied again.
    assert_eq!(
        app.world()
            .resource::<Assets<StandardMaterial>>()
            .get(&std_h0)
            .unwrap()
            .emissive,
        LinearRgba::new(4., 2.5, 0., 1.)
    );
    assert_eq!(
        app.world()
            .resource::<Assets<MToonMaterial>>()
            .get(&toon_h0)
            .unwrap()
            .base
            .emissive,
        LinearRgba::new(4., 2.5, 0., 1.)
    );

    // Removing manual key restores blended VRMA weight (0.25)
    app.world_mut()
        .get_mut::<ExpressionWeights>(rigs[0].0)
        .unwrap()
        .remove(&ExpressionName::Preset(ExpressionPresetName::Happy));
    app.update();
    let ch = app
        .world()
        .get::<expressions::Binds>(rigs[0].0)
        .unwrap()
        .target(&ExpressionName::Preset(ExpressionPresetName::Happy))
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
    let params = schema::ToonParams::from(&toon.schema);
    assert_eq!(params.shade_color_factor, Vec3::ZERO);
    assert_eq!(params.shading_toony_factor, 0.9);
    assert_eq!(toon.outline_mode(), schema::OutlineMode::None);
    assert_eq!(schema::ToonParams::min_size().get(), 400);

    let gaze: schema::LookAt = serde_json::from_value(json!({"type":"expression"})).unwrap();
    assert_eq!(
        runtime::look_at::remap(None, schema::LookAtType::Expression, 90.),
        1.
    );
    assert_eq!(
        runtime::look_at::head_offset(&gaze),
        Vec3::new(0., 0.06, 0.)
    );
    let expression_missing_scale: schema::LookAt = serde_json::from_value(json!({
        "type":"expression",
        "rangeMapHorizontalOuter":{"inputMaxValue":90.}
    }))
    .unwrap();
    assert_eq!(
        runtime::look_at::remap(
            expression_missing_scale.range_map_horizontal_outer.as_ref(),
            schema::LookAtType::Expression,
            90.
        ),
        1.
    );
    let bone_missing_scale: schema::LookAt = serde_json::from_value(json!({
        "type":"bone",
        "rangeMapHorizontalOuter":{"inputMaxValue":90.}
    }))
    .unwrap();
    assert_eq!(
        runtime::look_at::remap(
            bone_missing_scale.range_map_horizontal_outer.as_ref(),
            schema::LookAtType::Bone,
            90.
        ),
        10.
    );
    let motion: schema::Motion =
        serde_json::from_value(json!({"specVersion":"1.0","lookAt":{"node":3}})).unwrap();
    assert_eq!(motion.look_at.unwrap().node.unwrap().value(), 3);

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
    .save(files.join("tex.png"))
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
    let png = std::fs::read(files.join("tex.png")).unwrap();
    let image_offset = data.len();
    data.extend_from_slice(&png);
    let length = data.len();
    std::fs::write(files.join("tri.bin"), data).unwrap();

    let mut value = files.model();
    value["buffers"] = json!([{"uri":"tri.bin","byteLength":length}]);
    value["bufferViews"] = json!([
        {"buffer":0,"byteLength":36},
        {"buffer":0,"byteOffset":36,"byteLength":12},
        {"buffer":0,"byteOffset":48,"byteLength":48},
        {"buffer":0,"byteOffset":image_offset,"byteLength":png.len()}
    ]);
    value["accessors"] = json!([
        {"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]},
        {"bufferView":1,"componentType":5121,"count":3,"type":"VEC4"},
        {"bufferView":2,"componentType":5126,"count":3,"type":"VEC4"}
    ]);
    value["images"] = json!([{"bufferView":3,"mimeType":"image/png"}]);
    value["textures"] = json!([{"source":0}]);
    value["meshes"] = json!([{"primitives":[
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":0},
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":1},
        {"attributes":{"POSITION":0,"JOINTS_0":1,"WEIGHTS_0":2},"material":2}
    ]}]);
    value["skins"] = json!([{"joints":[1]}]);
    value["nodes"][0]["skin"] = json!(0);
    value["nodes"][0]["mesh"] = json!(0);
    value["scenes"][0]["nodes"] = json!([0]);
    value["materials"] = json!([
        {
            "alphaMode":"OPAQUE",
            "doubleSided":true,
            "emissiveFactor":[1.,0.5,0.],
            "occlusionTexture":{"index":0},
            "pbrMetallicRoughness":{"baseColorTexture":{"index":0,"extensions":{"KHR_texture_transform":{"offset":[0.5,0.25]}}}},
            "extensions":{
                "KHR_materials_unlit":{},
                "KHR_materials_emissive_strength":{"emissiveStrength":10.0},
                "VRMC_materials_mtoon":{"specVersion":"1.0","outlineWidthMode":"screenCoordinates","outlineWidthFactor":0.001,"renderQueueOffsetNumber":0,"shadeMultiplyTexture":{"index":0},"shadingShiftTexture":{"index":0}}
            }
        },
        {
            "alphaMode":"BLEND",
            "extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0","renderQueueOffsetNumber":-3}}
        },
        {
            "alphaMode":"BLEND",
            "extensions":{"VRMC_materials_mtoon":{"specVersion":"1.0","transparentWithZWrite":true,"renderQueueOffsetNumber":2}}
        }
    ]);
    value["extensions"]["VRMC_vrm"]["meta"] = json!({
        "name":"Ene Avatar",
        "authors":["Test Author"],
        "licenseUrl":"https://vrm.dev/licenses/1.0/",
        "commercialUsage":"personalNonProfit"
    });
    let path = files.write("full.vrm", &value);
    let handle: Handle<Gltf> = app
        .world()
        .resource::<AssetServer>()
        .load_builder()
        .with_settings(|s: &mut bevy::gltf::GltfLoaderSettings| {
            s.convert_coordinates = Some(bevy::gltf::convert_coordinates::GltfConvertCoordinates {
                rotate_scene_entity: false,
                rotate_meshes: true,
            });
        })
        .load(path.clone());
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

    let (shade, shift) = {
        let materials = app.world().resource::<Assets<MToonMaterial>>();
        let mat0 = materials.get(&m0).unwrap();
        assert!(
            !mat0.base.unlit,
            "MToon must keep Bevy's lighting and emission inputs enabled"
        );
        // Bevy folds `KHR_materials_emissive_strength` into the loaded color;
        // expressions must keep the factor to scale their targets once.
        assert_eq!(mat0.base.emissive.to_vec3(), Vec3::new(10., 5., 0.));
        assert_eq!(mat0.base.depth_bias, 0.0);
        assert_eq!(mat0.base.uv_transform.translation, Vec2::new(0.5, 0.25));
        assert!(mat0.extension.shift.is_some());
        assert_eq!(mat0.extension.params.texture_uvs, 0);
        assert_eq!(
            mat0.extension.params.uv_transforms[1],
            bevy::math::Mat3::IDENTITY
        );
        // MToon transparent queue offsets and ZWrite are delegated to Bevy's
        // standard transparent drawing.
        assert_eq!(materials.get(&m1).unwrap().base.depth_bias, 0.0);
        assert_eq!(materials.get(&m2).unwrap().base.depth_bias, 0.0);
        (mat0.extension.shade.clone(), mat0.extension.shift.clone())
    };
    let (base_color, occlusion) = {
        let materials = app.world().resource::<Assets<MToonMaterial>>();
        let mat0 = materials.get(&m0).unwrap();
        (
            mat0.base.base_color_texture.clone(),
            mat0.base.occlusion_texture.clone(),
        )
    };
    {
        let images = app.world().resource::<Assets<Image>>();
        let shade = images.get(shade.as_ref().unwrap()).unwrap();
        assert_eq!(
            shade.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        let shift = images.get(shift.as_ref().unwrap()).unwrap();
        assert_eq!(shift.texture_descriptor.format, TextureFormat::Rgba8Unorm);
        // The shared texture is linear because of the occlusion slot, so the
        // MToon color slots must ask for their own sRGB image.
        let base_color = images.get(base_color.as_ref().unwrap()).unwrap();
        assert_eq!(
            base_color.texture_descriptor.format,
            TextureFormat::Rgba8UnormSrgb
        );
        let occlusion = images.get(occlusion.as_ref().unwrap()).unwrap();
        assert_eq!(
            occlusion.texture_descriptor.format,
            TextureFormat::Rgba8Unorm
        );
    }

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
    assert_eq!(vrm.meta.name, "Ene Avatar");
    assert!(
        app.world_mut()
            .query::<&BaseUv>()
            .iter(app.world())
            .any(|uv| uv.0.offset == Vec2::new(0.5, 0.25)),
        "the authored base texture transform must survive scene instantiation"
    );
    let outlines = app
        .world_mut()
        .query_filtered::<&MeshMaterial3d<MToonMaterial>, With<Outline>>()
        .iter(app.world())
        .count();
    assert_eq!(outlines, 1);

    // Outlines are children of their body so visibility and despawn are
    // inherited; their local transform is identity.
    let mut bodies: HashMap<Entity, (usize, GlobalTransform)> = HashMap::default();
    let mut outline_entities = Vec::new();
    let mut query = app.world_mut().query::<(
        Entity,
        &ChildOf,
        &MaterialSlot,
        &Transform,
        &GlobalTransform,
        Option<&Outline>,
    )>();
    for (entity, parent, slot, transform, global, outline) in query.iter(app.world()) {
        if outline.is_some() {
            outline_entities.push((entity, parent.parent(), slot.0, *transform, *global));
        } else {
            bodies.insert(entity, (slot.0, *global));
        }
    }
    assert_eq!(outline_entities.len(), 1);
    for (_, parent, slot, transform, global) in &outline_entities {
        let (body_slot, body_global) = bodies.get(parent).expect("outline parent is the body");
        assert_eq!(*body_slot, *slot);
        assert_eq!(*transform, Transform::IDENTITY);
        assert_eq!(*global, *body_global);
    }
    let strengths: HashMap<usize, f32> = app
        .world_mut()
        .query::<(&MaterialSlot, &EmissiveStrength)>()
        .iter(app.world())
        .map(|(slot, strength)| (slot.0, strength.0))
        .collect();
    assert_eq!(strengths.get(&0), Some(&10.));
    assert_eq!(strengths.get(&1), Some(&1.));
    assert_eq!(strengths.get(&2), Some(&1.));

    let (outline, body) = (outline_entities[0].0, outline_entities[0].1);
    app.world_mut()
        .entity_mut(body)
        .insert((Visibility::Hidden, RenderLayers::layer(3)));
    app.update();
    let visibility = app.world().get::<InheritedVisibility>(outline).unwrap();
    assert!(!visibility.get(), "outline ignores the body's visibility");
    assert_eq!(
        app.world().get::<RenderLayers>(outline).unwrap(),
        &RenderLayers::layer(3)
    );
}

#[test]
#[ignore = "requires a GPU; runs in CI with lavapipe"]
fn synthetic_mtoon_uv1_outline_pipeline_smoke() {
    use bevy::mesh::PrimitiveTopology;
    use bevy::render::{
        RenderApp,
        render_resource::{CachedPipelineState, PipelineCache},
    };
    let mut app = gpu_app(ClearColor(Color::NONE));

    let color_image = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new(
            Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![
                255, 0, 0, 255, // red
                0, 255, 0, 255, // green
                0, 0, 255, 255, // blue
                255, 255, 0, 255, // yellow
            ],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        ));
    let mask_image = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new(
            Extent3d {
                width: 2,
                height: 2,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![0, 255, 0, 255, 0, 64, 0, 255, 0, 0, 0, 128, 0, 192, 0, 255],
            TextureFormat::Rgba8Unorm,
            RenderAssetUsages::default(),
        ));
    let normal_image = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new(
            Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![128, 128, 255, 255],
            TextureFormat::Rgba8Unorm,
            RenderAssetUsages::default(),
        ));
    let converted = HashMap::from([((0usize, TextureSpace::Linear), mask_image)]);
    let toon: schema::Toon = serde_json::from_value(json!({
        "specVersion":"1.0",
        "shadeMultiplyTexture":{"index":0,"texCoord":1},
        "uvAnimationMaskTexture":{"index":0,"texCoord":1},
        "outlineWidthMode":"screenCoordinates",
        "outlineWidthFactor":0.01,
        "outlineWidthMultiplyTexture":{"index":0,"texCoord":1}
    }))
    .unwrap();
    let body_material =
        app.world_mut()
            .resource_mut::<Assets<MToonMaterial>>()
            .add(MToonMaterial {
                base: StandardMaterial {
                    alpha_mode: bevy::material::AlphaMode::Mask(0.5),
                    base_color_texture: Some(color_image.clone()),
                    normal_map_texture: Some(normal_image),
                    ..default()
                },
                extension: MToonExtension::new(
                    &toon,
                    std::slice::from_ref(&color_image),
                    &converted,
                ),
            });
    let mut outline_extension =
        MToonExtension::new(&toon, std::slice::from_ref(&color_image), &converted);
    outline_extension.outline = Some(schema::OutlineMode::ScreenCoordinates);
    let outline_material =
        app.world_mut()
            .resource_mut::<Assets<MToonMaterial>>()
            .add(MToonMaterial {
                base: StandardMaterial {
                    cull_mode: Some(bevy::render::render_resource::Face::Back),
                    ..default()
                },
                extension: outline_extension,
            });

    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::default(),
    );
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_POSITION,
        vec![[-1., -1., 0.], [1., -1., 0.], [0., 1., 0.]],
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0., 0., 1.]; 3]);
    // UV0 and UV1 differ so the MToon texture coordinate selection is
    // exercised instead of comparing equal coordinates.
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0., 0.], [1., 0.], [0., 1.]]);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, vec![[0., 0.], [0., 1.], [1., 0.]]);
    // Exercise the tangent/normal-map, skinning and morph shader variants.
    mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, vec![[1., 0., 0., 1.]; 3]);
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_JOINT_INDEX,
        VertexAttributeValues::Uint16x4(vec![[0u16; 4]; 3]),
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, vec![[1., 0., 0., 0.]; 3]);
    mesh.set_morph_targets(vec![
        MorphAttributes::new(Vec3::ZERO, Vec3::ZERO, Vec3::ZERO);
        3
    ]);
    let mesh = app.world_mut().resource_mut::<Assets<Mesh>>().add(mesh);
    let joint = app.world_mut().spawn(Transform::default()).id();
    let inverse_bindposes = app
        .world_mut()
        .resource_mut::<Assets<SkinnedMeshInverseBindposes>>()
        .add(SkinnedMeshInverseBindposes::from(vec![Mat4::IDENTITY]));
    let skin = SkinnedMesh {
        inverse_bindposes,
        joints: vec![joint],
    };
    let morph = MeshMorphWeights::Value { weights: vec![0.5] };
    let body = app
        .world_mut()
        .spawn((
            Mesh3d(mesh.clone()),
            MeshMaterial3d(body_material),
            Visibility::Visible,
            NoFrustumCulling,
            skin.clone(),
            morph.clone(),
        ))
        .id();
    app.world_mut().spawn((
        Mesh3d(mesh),
        MeshMaterial3d(outline_material),
        Visibility::Visible,
        NoFrustumCulling,
        Transform::IDENTITY,
        ChildOf(body),
        skin,
        morph,
    ));

    let target = app
        .world_mut()
        .resource_mut::<Assets<Image>>()
        .add(Image::new_target_texture(
            64,
            64,
            TextureFormat::Rgba8UnormSrgb,
            None,
        ));
    app.world_mut().spawn((
        Camera3d::default(),
        bevy::core_pipeline::tonemapping::Tonemapping::Linear,
        bevy::camera::RenderTarget::Image(target.into()),
        Transform::from_xyz(0., 0., 3.).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    app.world_mut().spawn((
        DirectionalLight::default(),
        Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.4, -0.3, 0.)),
    ));
    for _ in 0..30 {
        app.update();
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
    let entities: Vec<Entity> = app
        .world_mut()
        .query_filtered::<Entity, With<MeshMaterial3d<MToonMaterial>>>()
        .iter(app.world())
        .collect();
    assert_eq!(entities.len(), 2, "body and outline materials");
    assert_specialized_pipelines_ok(&app, &entities);
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
