use super::*;
use bevy::time::TimeUpdateStrategy;
use ene_vrm::{SpringRig, Vrm};
use std::time::Instant;

struct Harness {
    app: App,
    input: SyncSender<Result<Request, String>>,
    output: Receiver<Value>,
}

impl Harness {
    fn new() -> Self {
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
        .register_asset_loader(bevy::image::ImageLoader::new(
            bevy::image::CompressedImageFormats::NONE,
        ))
        .add_plugins((
            bevy::extract::sync_world::SyncWorldPlugin::<bevy::render::RenderApp>::default(),
            ene_vrm::VrmPlugin,
            crate::character::setup,
            crate::desktop::setup,
        ))
        .add_systems(PreUpdate, requests)
        .add_systems(PostUpdate, publish.after(crate::character::notify_finished));
        let (input, rx) = mpsc::sync_channel(64);
        let (tx, output) = mpsc::sync_channel(64);
        app.insert_non_send(rx).insert_resource(Control {
            output: tx,
            fps: 30,
            #[cfg(target_os = "linux")]
            wake_tx: exwlshellev::calloop::channel::channel().0,
        });
        app.world_mut().spawn(Output {
            name: "screen".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        });
        app.finish();
        app.cleanup();
        Self { app, input, output }
    }

    fn batch(&mut self, commands: &[Value]) -> Vec<Value> {
        for command in commands {
            self.input
                .send(serde_json::from_value(command.clone()).map_err(|e| e.to_string()))
                .unwrap();
        }
        self.app.update();
        self.output.try_iter().collect()
    }

    fn send(&mut self, command: Value) -> Value {
        self.batch(&[command])
            .into_iter()
            .find(|v| v.get("id").is_some())
            .unwrap()
    }

    fn wait(&mut self, expected: &[(&str, &str)]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = self.send(json!({"id":0,"op":"get"}));
            let characters = state["ok"]["characters"].as_array().unwrap();
            if expected.iter().all(|(id, status)| {
                characters
                    .iter()
                    .any(|c| c["character"] == *id && c["status"] == *status)
            }) {
                return;
            }
            assert!(Instant::now() < deadline, "states did not settle: {state}");
            std::thread::yield_now();
        }
    }
}

#[test]
fn input_is_bounded_and_validated() {
    for (line, valid, end) in [
        ("{\"id\":1,\"op\":\"get\"}".to_owned(), true, false),
        ("".to_owned(), true, true),
        ("{".to_owned(), false, false),
        (" ".repeat(65_537), false, true),
        (
            format!("{{\"id\":1,\"op\":\"get\"}}{}", " ".repeat(65_517)),
            true,
            false,
        ),
        (
            "{\"id\":1,\"op\":\"get\",\"typo\":true}".to_owned(),
            false,
            false,
        ),
    ] {
        let (request, stopped) = read_request(&mut line.as_bytes());
        assert_eq!((request.is_ok(), stopped), (valid, end));
    }
    for config in [
        json!({"model":"bad.txt"}),
        json!({"model":"a.vrm","motion":"bad.vrm"}),
        json!({"model":"a.vrm","size_percent":49}),
        json!({"model":"a.vrm","size_percent":201}),
        json!({"model":"a.vrm","position":[1000001,0]}),
        json!({"model":"a.vrm","typo":true}),
    ] {
        assert!(
            serde_json::from_value::<Config>(config)
                .map_err(|e| e.to_string())
                .and_then(|c| c.validate())
                .is_err()
        );
    }
}

#[test]
fn commands_are_ordered_and_invalid_changes_are_atomic() {
    let mut h = Harness::new();
    let messages = h.batch(&[
        json!({"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}),
        json!({"id":2,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}),
        json!({"id":3,"op":"get"}),
        json!({"id":4,"op":"set","character":"a","config":{"model":"bad"}}),
        json!({"id":5,"op":"get"}),
        json!({"id":6,"op":"remove","character":"a"}),
        json!({"id":7,"op":"get"}),
    ]);
    assert_eq!(
        messages[..7]
            .iter()
            .map(|v| v["id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    let config = &messages[2]["ok"]["characters"][0]["config"];
    assert_eq!(config["playing"], false);
    assert_eq!(config["monitor"], "screen");
    assert_eq!(config["position"], json!([50., 80.]));
    assert!(messages[3]["error"].is_string());
    assert_eq!(messages[2]["ok"], messages[4]["ok"]);
    assert_eq!(messages[6]["ok"]["characters"], json!([]));
    assert_eq!(messages[7]["status"], "loading");
    for command in [
        json!({"id":8,"op":"add","character":"","config":{"model":"a.vrm"}}),
        json!({"id":8,"op":"add","character":"a".repeat(129),"config":{"model":"a.vrm"}}),
        json!({"id":8,"op":"remove","character":"missing"}),
        json!({"id":8,"op":"set","character":"missing","config":{"model":"a.vrm"}}),
        json!({"id":8,"op":"center","character":"missing"}),
        json!({"id":8,"op":"fps","value":1001}),
    ] {
        assert!(h.send(command)["error"].is_string());
    }
    h.send(json!({"id":9,"op":"add","character":"a","config":{"model":"a.vrm","position":[1,2]}}));
    assert!(
        h.send(json!({"id":10,"op":"add","character":"a","config":{"model":"a.vrm"}}))["error"]
            .is_string()
    );
    h.send(json!({"id":11,"op":"center","character":"a","monitor":"missing"}));
    h.send(json!({"id":12,"op":"fps","value":0}));
    let state = h.send(json!({"id":13,"op":"get"}));
    assert_eq!(state["ok"]["fps"], 0);
    assert_eq!(
        state["ok"]["characters"][0]["config"]["position"],
        json!([50., 80.])
    );
    h.send(json!({"id":14,"op":"quit"}));
    assert_eq!(h.app.should_exit(), Some(AppExit::Success));
}

#[test]
fn output_failures_exit_and_shutdown_drains_replies() {
    for disconnected in [false, true] {
        let mut h = Harness::new();
        if disconnected {
            drop(h.output);
        } else {
            for _ in 0..64 {
                h.app
                    .world()
                    .resource::<Control>()
                    .output
                    .try_send(Value::Null)
                    .unwrap();
            }
        }
        h.input
            .send(Ok(Request {
                id: 1,
                command: Command::Get {},
            }))
            .unwrap();
        h.app.update();
        assert_eq!(h.app.should_exit(), Some(AppExit::error()));
    }
    let mut app = App::new();
    let (tx, rx) = mpsc::sync_channel(64);
    let (done, completion) = mpsc::channel();
    app.insert_non_send(completion).set_runner(move |_| {
        tx.send(json!({"id":1,"ok":null})).unwrap();
        AppExit::Success
    });
    let writer = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        done.send(write_output(&mut bytes, rx)).unwrap();
        bytes
    });
    assert_eq!(run(app), AppExit::Success);
    assert_eq!(
        serde_json::from_slice::<Value>(&writer.join().unwrap()).unwrap(),
        json!({"id":1,"ok":null})
    );
    let (tx, rx) = mpsc::sync_channel(1);
    tx.send(Value::Null).unwrap();
    drop(tx);
    assert!(write_output(std::io::Cursor::new(&mut [][..]), rx).is_err());
}

#[test]
fn assets_retry_playback_and_removal() {
    use tempfile::TempDir;
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let directory = TempDir::new().unwrap();
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
    let values: Vec<f32> = vec![0., 1., 0., 1., 0., 0., 1., 0.];
    std::fs::write(
        directory.path().join("motion.bin"),
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
    motion_value["accessors"] = json!([
        {"bufferView":0,"componentType":5126,"count":2,"type":"SCALAR","min":[0],"max":[1]},
        {"bufferView":1,"componentType":5126,"count":2,"type":"VEC3"}
    ]);
    motion_value["animations"] = json!([{
        "samplers":[{"input":0,"output":1}],
        "channels":[{"sampler":0,"target":{"node":0,"path":"translation"}}]
    }]);
    let mut textured_model = model.clone();
    textured_model["images"] = json!([{"uri":"skin.png"}]);
    textured_model["textures"] = json!([{"source":0}]);
    textured_model["materials"] = json!([{
        "pbrMetallicRoughness":{"baseColorTexture":{"index":0}}
    }]);

    let good = directory.path().join("good.vrm");
    let good2 = directory.path().join("good2.vrm");
    let bad = directory.path().join("bad.vrm");
    let motion = directory.path().join("motion.vrma");
    let bad_motion = directory.path().join("bad.vrma");
    let textured = directory.path().join("textured.vrm");
    let skin = directory.path().join("skin.png");
    std::fs::write(&good, model.to_string()).unwrap();
    std::fs::write(&good2, model.to_string()).unwrap();
    std::fs::write(&bad, "{}").unwrap();
    std::fs::write(&motion, motion_value.to_string()).unwrap();
    std::fs::write(&bad_motion, "{}").unwrap();
    std::fs::write(&textured, textured_model.to_string()).unwrap();
    std::fs::write(&skin, b"not a png").unwrap();

    let mut h = Harness::new();
    let assets = [
        (&good, Some(&motion)),
        (&good, Some(&motion)),
        (&bad, None),
        (&good2, Some(&bad_motion)),
        (&textured, None),
    ];
    for (id, (model, motion)) in assets.iter().enumerate() {
        assert_eq!(h.send(json!({"id":1,"op":"add","character":id.to_string(),"config":{"model":model,"motion":motion,"playing":false}})), json!({"id":1,"ok":null}));
    }
    h.wait(&[
        ("0", "ready"),
        ("1", "ready"),
        ("2", "error"),
        ("3", "error"),
        ("4", "error"),
    ]);
    std::fs::write(&bad, model.to_string()).unwrap();
    std::fs::write(&bad_motion, motion_value.to_string()).unwrap();
    std::fs::write(&skin, PNG).unwrap();
    for id in [2, 3, 4] {
        let (model, motion) = assets[id];
        h.send(json!({"id":2,"op":"set","character":id.to_string(),"config":{"model":model,"motion":motion}}));
    }
    h.wait(&[("2", "ready"), ("3", "ready"), ("4", "ready")]);
    let root = h
        .app
        .world_mut()
        .query::<&Character>()
        .iter(h.app.world())
        .find(|c| c.id == "0")
        .unwrap()
        .root
        .unwrap();
    h.app
        .world_mut()
        .entity_mut(root)
        .insert(SpringRig::default());
    h.app.update();
    assert!(h.app.world().get::<SpringRig>(root).unwrap().paused);
    assert!(
        h.app
            .world()
            .get::<AnimationPlayer>(root)
            .unwrap()
            .all_paused()
    );
    h.app
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs(1)));
    let play = |looping| json!({"id":3,"op":"set","character":"0","config":{"model":good,"motion":motion,"looping":looping}});
    h.send(play(true));
    assert!(!h.app.world().get::<SpringRig>(root).unwrap().paused);
    let events = h.batch(&[play(false)]);
    assert_eq!(
        events
            .iter()
            .filter(|v| v["event"] == "finished" && v["character"] == "0")
            .count(),
        1
    );
    assert!(!h.batch(&[]).iter().any(|v| v["event"] == "finished"));
    let state = h.send(json!({"id":4,"op":"get"}));
    let paused = state["ok"]["characters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["character"] == "1")
        .unwrap();
    assert_eq!(paused["animation_times"], json!([0.]));
    let outputs: Vec<_> = h
        .app
        .world_mut()
        .query_filtered::<Entity, With<Output>>()
        .iter(h.app.world())
        .collect();
    for output in outputs {
        h.app.world_mut().despawn(output);
    }
    h.wait(&[("0", "suspended"), ("1", "suspended")]);
    h.app.world_mut().spawn(Output {
        name: "back".into(),
        origin: Vec2::ZERO,
        size: Vec2::splat(100.),
        units_per_logical_pixel: 1.,
    });
    h.wait(&[("0", "ready"), ("1", "ready")]);
    h.send(json!({"id":5,"op":"set","character":"0","config":{"model":good2}}));
    h.wait(&[("0", "ready")]);
    assert_eq!(
        h.app
            .world_mut()
            .query::<&Vrm>()
            .iter(h.app.world())
            .count(),
        5
    );
    for id in 0..5 {
        h.send(json!({"id":6,"op":"remove","character":id.to_string()}));
    }
    assert_eq!(
        h.app
            .world_mut()
            .query::<&Vrm>()
            .iter(h.app.world())
            .count(),
        0
    );
}
