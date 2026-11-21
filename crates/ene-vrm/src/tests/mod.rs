mod materials;
mod scene;

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
        v["extensions"] = json!({"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":humanoid}});
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
