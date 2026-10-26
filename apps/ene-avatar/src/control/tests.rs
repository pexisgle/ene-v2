use super::*;
use super::{
    output::{publish, write_output},
    protocol::{Command, Request},
    read_request,
    session::requests,
};
use crate::character::{AvatarState, CharacterId, Config, ModelAssets, Notification};
use crate::desktop::Output;
use bevy::app::AppExit;
use bevy::asset::{LoadState, RecursiveDependencyLoadState};
use bevy::prelude::*;
use ene_vrm::Vrm;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Harness {
    app: App,
    input: SyncSender<Result<Request, String>>,
    output: Receiver<Value>,
    events: VecDeque<Value>,
}

fn harness(app: App) -> Harness {
    let (input, input_rx) = mpsc::sync_channel(64);
    let (output, output_rx) = mpsc::sync_channel(64);
    let mut app = app;
    app.insert_non_send(input_rx).insert_resource(Control {
        output,
        fps: 30,
        #[cfg(target_os = "linux")]
        wake_tx: exwlshellev::calloop::channel::channel().0,
    });
    Harness {
        app,
        input,
        output: output_rx,
        events: VecDeque::new(),
    }
}

impl Harness {
    fn send(&mut self, request: Result<Request, String>) -> Value {
        self.input.send(request).unwrap();
        self.app.update();
        loop {
            let message = self
                .output
                .try_recv()
                .expect("app.update() must produce a command reply");
            if message.get("id").is_some() {
                return message;
            }
            self.events.push_back(message);
        }
    }

    fn line(&mut self, line: &str) -> Value {
        let request: Result<Request, String> =
            serde_json::from_str(line).map_err(|error| error.to_string());
        self.send(request)
    }

    fn batch(&mut self, lines: &[&str]) -> Vec<Value> {
        for line in lines {
            let request: Result<Request, String> =
                serde_json::from_str(line).map_err(|error| error.to_string());
            self.input.send(request).unwrap();
        }
        self.app.update();
        self.output.try_iter().collect()
    }

    fn event(&mut self) -> Value {
        if let Some(event) = self.events.pop_front() {
            return event;
        }
        let message = self
            .output
            .try_recv()
            .expect("app.update() must produce a notification");
        assert!(message.get("id").is_none(), "unexpected reply: {message}");
        message
    }

    fn no_event(&mut self) -> bool {
        self.events.is_empty() && self.output.try_recv().is_err()
    }

    fn drain(&mut self) -> Vec<Value> {
        let mut values: Vec<Value> = self.events.drain(..).collect();
        values.extend(self.output.try_iter());
        values
    }

    fn wait_for_states(&mut self, states: &mut HashMap<String, String>, expected: &[(&str, &str)]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stable = 0;
        loop {
            for message in self.drain() {
                if message["event"] != "state" {
                    continue;
                }
                if message["status"] == "error" {
                    assert!(message["error"].is_string(), "missing error: {message}");
                }
                states.insert(
                    message["character"].as_str().unwrap().to_owned(),
                    message["status"].as_str().unwrap().to_owned(),
                );
            }
            if expected.iter().all(|(character, status)| {
                states.get(*character).map(String::as_str) == Some(*status)
            }) {
                stable += 1;
                if stable == 3 {
                    return;
                }
            } else {
                stable = 0;
            }
            assert!(
                Instant::now() < deadline,
                "states did not settle: {states:?}"
            );
            self.app.update();
            std::thread::yield_now();
        }
    }

    fn character(&mut self, id: &str) -> Entity {
        self.app
            .world_mut()
            .query::<(Entity, &CharacterId)>()
            .iter(self.app.world())
            .find(|(_, character)| character.0 == id)
            .map(|(entity, _)| entity)
            .expect("character exists")
    }
}

fn spawn_output(app: &mut App, name: &str, size: Vec2, scale: f32) -> Entity {
    app.world_mut()
        .spawn(Output {
            name: name.into(),
            origin: Vec2::ZERO,
            size,
            units_per_logical_pixel: scale,
        })
        .id()
}

fn control_app() -> Harness {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<bevy::gltf::Gltf>()
        .init_asset::<WorldAsset>()
        .init_asset::<AnimationGraph>()
        .init_asset::<AnimationClip>()
        .add_message::<Notification>()
        .add_systems(PreUpdate, requests)
        .add_systems(PostUpdate, publish);
    harness(app)
}

fn native_app() -> Harness {
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
    ))
    .add_systems(PreUpdate, requests)
    .add_systems(PostUpdate, publish);
    let mut harness = harness(app);
    harness.app.finish();
    harness.app.cleanup();
    harness
}

fn request(value: Value) -> Result<Request, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

fn rejected(harness: &mut Harness, line: &str) {
    let reply = harness.line(line);
    assert!(reply["error"].is_string(), "expected rejection: {reply}");
}

#[test]
fn input_framing_and_eof() {
    let mut valid = std::io::Cursor::new(b"{\"id\":1,\"op\":\"get\"}\n".to_vec());
    let (request, end) = read_request(&mut valid);
    assert!(!end);
    let request = request.unwrap();
    assert_eq!(request.id, 1);
    assert!(matches!(request.command, Command::Get));

    let mut malformed = std::io::Cursor::new(b"{\n".to_vec());
    let (request, end) = read_request(&mut malformed);
    assert!(request.is_err());
    assert!(!end);

    let json = r#"{"id":1,"op":"get"}"#;
    let mut boundary = String::from(json);
    boundary.push_str(&" ".repeat(65_536 - json.len() - 1));
    boundary.push('\n');
    assert_eq!(boundary.len(), 65_536);
    let mut boundary = std::io::Cursor::new(boundary.into_bytes());
    let (request, end) = read_request(&mut boundary);
    assert!(request.is_ok());
    assert!(!end);

    let mut oversized = std::io::Cursor::new(vec![b' '; 65_537]);
    let (request, end) = read_request(&mut oversized);
    assert!(request.is_err());
    assert!(end);

    let mut eof = std::io::Cursor::new(Vec::new());
    let (request, end) = read_request(&mut eof);
    assert!(end);
    let request = request.unwrap();
    assert_eq!(request.id, 0);
    assert!(matches!(request.command, Command::Quit));
}

#[test]
fn command_contract() {
    let mut harness = control_app();
    spawn_output(&mut harness.app, "m", Vec2::splat(100.), 1.);

    assert_eq!(
        harness.line(r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#),
        json!({"id": 1, "ok": null})
    );
    assert_eq!(
        harness.line(r#"{"id":2,"op":"get"}"#),
        json!({
            "id": 2,
            "ok": {
                "characters": [{
                    "character": "a",
                    "status": "loading",
                    "error": null,
                    "config": {
                        "model": "a.vrm",
                        "motion": null,
                        "playing": true,
                        "looping": true,
                        "monitor": "m",
                        "position": [50., 80.],
                        "size_percent": 100.
                    },
                    "animation_times": []
                }],
                "monitors": [{
                    "name": "m",
                    "origin": [0., 0.],
                    "size": [100., 100.],
                    "units_per_logical_pixel": 1.
                }],
                "fps": 30
            }
        })
    );

    let entity = harness.character("a");
    let mut player = AnimationPlayer::default();
    player
        .play(AnimationNodeIndex::new(0))
        .repeat()
        .seek_to(1.25);
    let root = harness
        .app
        .world_mut()
        .spawn((player, ChildOf(entity)))
        .id();
    harness
        .app
        .world_mut()
        .entity_mut(entity)
        .insert(ModelAssets::with_root(root));
    let get = harness.line(r#"{"id":17,"op":"get"}"#);
    assert_eq!(get["ok"]["characters"][0]["animation_times"], json!([1.25]));
    assert_eq!(get["ok"]["characters"][0].get("spring_paused"), None);

    harness
        .line(r#"{"id":3,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}"#);
    let config = harness.app.world().get::<Config>(entity).unwrap();
    assert_eq!(config.model, "a.vrm");
    assert!(!config.playing);
    assert_eq!(config.motion, None);
    assert!(config.looping);
    assert_eq!(config.size_percent, 100.);
    let get = harness.line(r#"{"id":4,"op":"get"}"#);
    assert_eq!(get["ok"]["characters"][0]["config"]["playing"], false);
    assert_eq!(get["ok"]["characters"][0]["config"]["looping"], true);
    assert_eq!(get["ok"]["characters"][0]["config"]["size_percent"], 100.);

    harness.line(r#"{"id":5,"op":"center","character":"a","monitor":"missing"}"#);
    let get = harness.line(r#"{"id":6,"op":"get"}"#);
    assert_eq!(get["ok"]["characters"][0]["config"]["monitor"], "m");
    assert_eq!(
        get["ok"]["characters"][0]["config"]["position"],
        json!([50., 80.])
    );
    assert_eq!(
        harness.app.world().get::<Config>(entity).unwrap().monitor,
        Some("missing".into())
    );

    harness.line(r#"{"id":7,"op":"fps","value":0}"#);
    assert_eq!(harness.line(r#"{"id":8,"op":"get"}"#)["ok"]["fps"], 0);
    harness.line(r#"{"id":9,"op":"fps","value":60}"#);
    assert_eq!(harness.line(r#"{"id":10,"op":"get"}"#)["ok"]["fps"], 60);

    harness.line(
        r#"{"id":11,"op":"add","character":"b","config":{"model":"b.vrm","monitor":"m","position":[10,20],"size_percent":150}}"#,
    );
    let get = harness.line(r#"{"id":12,"op":"get"}"#);
    let characters = get["ok"]["characters"].as_array().unwrap();
    assert_eq!(characters.len(), 2);
    let b = characters
        .iter()
        .find(|character| character["character"] == "b")
        .unwrap();
    assert_eq!(b["config"]["monitor"], "m");
    assert_eq!(b["config"]["position"], json!([10., 20.]));
    assert_eq!(b["config"]["size_percent"], 150.);

    harness.line(r#"{"id":13,"op":"remove","character":"a"}"#);
    assert_eq!(
        harness.line(r#"{"id":14,"op":"get"}"#)["ok"]["characters"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    harness.line(r#"{"id":15,"op":"remove","character":"b"}"#);
    assert!(
        harness.line(r#"{"id":16,"op":"get"}"#)["ok"]["characters"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn command_rejections() {
    let mut harness = control_app();

    for line in [
        r#"{"id":1,"op":"set","character":"missing","config":{"model":"a.vrm"}}"#,
        r#"{"id":2,"op":"remove","character":"missing"}"#,
        r#"{"id":3,"op":"center","character":"missing","monitor":null}"#,
        r#"{"id":4,"op":"add","character":"","config":{"model":"a.vrm"}}"#,
        r#"{"id":5,"op":"add","character":"a","config":{"model":"bad.txt"}}"#,
        r#"{"id":6,"op":"add","character":"a","config":{"model":"a.vrm","motion":"bad.vrm"}}"#,
        r#"{"id":7,"op":"add","character":"a","config":{"model":"a.vrm","size_percent":49}}"#,
        r#"{"id":8,"op":"add","character":"a","config":{"model":"a.vrm","position":[1000001,0]}}"#,
        r#"{"id":9,"op":"fps","value":1001}"#,
    ] {
        rejected(&mut harness, line);
    }

    let long_id = "a".repeat(129);
    rejected(
        &mut harness,
        &format!(r#"{{"id":10,"op":"add","character":"{long_id}","config":{{"model":"a.vrm"}}}}"#),
    );

    harness.line(r#"{"id":11,"op":"add","character":"a","config":{"model":"a.vrm"}}"#);
    rejected(
        &mut harness,
        r#"{"id":12,"op":"add","character":"a","config":{"model":"b.vrm"}}"#,
    );
    harness.line(r#"{"id":13,"op":"fps","value":1000}"#);

    let malformed: Result<Request, String> = Err("invalid JSON".into());
    let reply = harness.send(malformed);
    assert_eq!(reply["id"], 0);
    assert!(reply["error"].is_string());

    let unknown: Result<Request, String> =
        serde_json::from_str(r#"{"id":14,"op":"fps","value":30,"typo":true}"#)
            .map_err(|error: serde_json::Error| error.to_string());
    assert!(unknown.is_err());
    let reply = harness.send(unknown);
    assert_eq!(reply["id"], 0);
    assert!(reply["error"].is_string());

    let unknown_config: Result<Request, String> = serde_json::from_str(
        r#"{"id":15,"op":"add","character":"a","config":{"model":"a.vrm","typo":true}}"#,
    )
    .map_err(|error: serde_json::Error| error.to_string());
    assert!(unknown_config.is_err());
}

#[test]
fn batch_order() {
    let mut harness = control_app();
    spawn_output(&mut harness.app, "m", Vec2::splat(100.), 1.);
    let messages = harness.batch(&[
        r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#,
        r#"{"id":2,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}"#,
        r#"{"id":3,"op":"get"}"#,
        r#"{"id":4,"op":"add","character":"a","config":{"model":"b.vrm"}}"#,
        r#"{"id":5,"op":"remove","character":"a"}"#,
        r#"{"id":6,"op":"add","character":"a","config":{"model":"b.vrm","position":[5,6]}}"#,
        r#"{"id":7,"op":"get"}"#,
    ]);
    assert_eq!(messages.len(), 9);
    assert_eq!(messages[0], json!({"id": 1, "ok": null}));
    assert_eq!(messages[1], json!({"id": 2, "ok": null}));
    assert_eq!(messages[2]["id"], 3);
    assert!(messages[3]["error"].is_string());
    assert_eq!(messages[4], json!({"id": 5, "ok": null}));
    assert_eq!(messages[5], json!({"id": 6, "ok": null}));
    assert_eq!(messages[6]["id"], 7);
    assert_eq!(
        messages[2]["ok"]["characters"][0]["config"]["playing"],
        false
    );
    assert_eq!(
        messages[6]["ok"]["characters"][0]["config"]["model"],
        "b.vrm"
    );
    assert_eq!(
        messages[6]["ok"]["characters"][0]["config"]["position"],
        json!([5., 6.])
    );
    for message in &messages[7..] {
        assert_eq!(message["event"], "state");
        assert_eq!(message["character"], "a");
        assert_eq!(message["status"], "loading");
    }
}

#[test]
fn reload_policy() {
    let mut harness = control_app();
    spawn_output(&mut harness.app, "m", Vec2::splat(100.), 1.);

    harness.line(r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#);
    assert_eq!(harness.event()["status"], "loading");

    harness
        .line(r#"{"id":2,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}"#);
    assert!(harness.no_event());

    harness.line(
        r#"{"id":3,"op":"set","character":"a","config":{"model":"a.vrm","motion":"m.vrma","playing":false}}"#,
    );
    assert_eq!(harness.event()["status"], "loading");

    harness.line(
        r#"{"id":4,"op":"set","character":"a","config":{"model":"a.vrm","motion":"m.vrma","playing":false}}"#,
    );
    assert!(harness.no_event());

    harness.line(
        r#"{"id":5,"op":"set","character":"a","config":{"model":"b.vrm","motion":"m.vrma","playing":false}}"#,
    );
    assert_eq!(harness.event()["status"], "loading");

    let entity = harness.character("a");
    harness
        .app
        .world_mut()
        .entity_mut(entity)
        .insert(AvatarState::Error("broken".into()));
    harness.line(
        r#"{"id":6,"op":"set","character":"a","config":{"model":"b.vrm","motion":"m.vrma","playing":false}}"#,
    );
    assert_eq!(harness.event()["status"], "loading");
    assert_eq!(
        harness.app.world().get::<AvatarState>(entity),
        Some(&AvatarState::Loading)
    );
}

#[test]
fn shutdown_contract() {
    for exit in [AppExit::Success, AppExit::error()] {
        let expected = exit.clone();
        let mut app = App::new();
        let (output, output_rx) = mpsc::sync_channel(64);
        let (done, completion) = mpsc::channel();
        app.insert_non_send(completion);
        app.set_runner(move |_| {
            for id in 0..17 {
                output.try_send(json!({"id": id, "ok": null})).unwrap();
            }
            exit
        });
        let writer = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = write_output(&mut bytes, output_rx);
            done.send(result).unwrap();
            bytes
        });
        assert_eq!(run(app), expected);
        let bytes = writer.join().unwrap();
        let replies: Vec<Value> = serde_json::Deserializer::from_slice(&bytes)
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(replies.len(), 17);
        assert_eq!(replies[16]["id"], 16);
    }

    for mode in 0..3 {
        let mut app = App::new();
        app.set_runner(|_| AppExit::Success);
        let (done, completion) = mpsc::channel::<std::io::Result<()>>();
        app.insert_non_send(completion);
        let mut held = Some(done);
        match mode {
            0 => held
                .as_ref()
                .unwrap()
                .send(Err(std::io::ErrorKind::BrokenPipe.into()))
                .unwrap(),
            1 => drop(held.take()),
            _ => {}
        }
        assert_eq!(run(app), AppExit::error());
    }

    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(json!({"id": 1, "ok": null})).unwrap();
    drop(sender);
    let mut closed = std::io::Cursor::new(&mut [][..]);
    assert!(write_output(&mut closed, receiver).is_err());
}

#[test]
fn output_failure_exits() {
    let mut harness = control_app();
    let sender = harness.app.world().resource::<Control>().output.clone();
    for _ in 0..64 {
        sender.try_send(Value::Null).unwrap();
    }
    harness
        .input
        .send(Ok(Request {
            id: 1,
            command: Command::Get,
        }))
        .unwrap();
    harness.app.update();
    assert_eq!(harness.app.should_exit(), Some(AppExit::error()));

    let mut harness = control_app();
    let sender = harness.app.world().resource::<Control>().output.clone();
    for _ in 0..64 {
        sender.try_send(Value::Null).unwrap();
    }
    harness
        .app
        .world_mut()
        .write_message(Notification(json!({"event": "finished", "character": "a"})));
    harness.app.update();
    assert_eq!(harness.app.should_exit(), Some(AppExit::error()));

    let mut harness = control_app();
    drop(harness.output);
    harness
        .input
        .send(Ok(Request {
            id: 1,
            command: Command::Get,
        }))
        .unwrap();
    harness.app.update();
    assert_eq!(harness.app.should_exit(), Some(AppExit::error()));

    let mut harness = control_app();
    drop(harness.output);
    harness
        .app
        .world_mut()
        .write_message(Notification(json!({"event": "finished", "character": "a"})));
    harness.app.update();
    assert_eq!(harness.app.should_exit(), Some(AppExit::error()));
}

#[test]
fn native_asset_pipeline() {
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

    let mut harness = native_app();
    spawn_output(&mut harness.app, "test", Vec2::splat(100.), 1.);
    let actors = [
        (&good, Some(&motion)),
        (&bad, None),
        (&good, Some(&motion)),
        (&good2, Some(&bad_motion)),
        (&textured, None),
    ];
    for (index, (model, motion)) in actors.iter().enumerate() {
        let reply = harness.send(request(json!({
            "id": index + 1,
            "op": "add",
            "character": index.to_string(),
            "config": {
                "model": model.to_str().unwrap(),
                "motion": motion.map(|path| path.to_str().unwrap()),
            }
        })));
        assert_eq!(reply, json!({"id": index + 1, "ok": null}));
    }

    let mut states = HashMap::new();
    harness.wait_for_states(
        &mut states,
        &[
            ("0", "ready"),
            ("1", "error"),
            ("2", "ready"),
            ("3", "error"),
            ("4", "error"),
        ],
    );
    let entity = harness.character("4");
    let model_handle = harness
        .app
        .world()
        .get::<ModelAssets>(entity)
        .unwrap()
        .model_handle();
    let server = harness.app.world().resource::<AssetServer>();
    assert!(
        matches!(
            server.get_load_states(model_handle.id()),
            Some((
                LoadState::Loaded,
                _,
                RecursiveDependencyLoadState::Failed(_)
            ))
        ),
        "the textured model must fail through its broken image"
    );

    std::fs::write(&bad, model.to_string()).unwrap();
    std::fs::write(&bad_motion, motion_value.to_string()).unwrap();
    std::fs::write(&skin, PNG).unwrap();
    for index in [1, 3, 4] {
        states.remove(&index.to_string());
        let (model, motion) = actors[index];
        harness.send(request(json!({
            "id": 10 + index,
            "op": "set",
            "character": index.to_string(),
            "config": {
                "model": model.to_str().unwrap(),
                "motion": motion.map(|path| path.to_str().unwrap()),
            }
        })));
    }
    harness.wait_for_states(
        &mut states,
        &[("1", "ready"), ("3", "ready"), ("4", "ready")],
    );
    let entity = harness.character("4");
    let repaired = harness
        .app
        .world()
        .get::<ModelAssets>(entity)
        .unwrap()
        .model_handle();
    let server = harness.app.world().resource::<AssetServer>();
    assert!(
        server.is_loaded_with_dependencies(repaired.id()),
        "the repaired model must be ready with its image dependency"
    );

    states.remove("0");
    harness.send(request(json!({
        "id": 20,
        "op": "set",
        "character": "0",
        "config": {
            "model": good2.to_str().unwrap(),
            "motion": motion.to_str().unwrap(),
        }
    })));
    harness.wait_for_states(&mut states, &[("0", "ready")]);
    assert_eq!(
        harness
            .app
            .world_mut()
            .query::<&Vrm>()
            .iter(harness.app.world())
            .count(),
        5
    );

    let get = harness.send(request(json!({"id": 30, "op": "get"})));
    let characters = get["ok"]["characters"].as_array().unwrap();
    assert_eq!(characters.len(), 5);
    assert!(
        characters
            .iter()
            .all(|character| character["status"] == "ready")
    );

    for index in 0..5 {
        let reply = harness.send(request(json!({
            "id": 40 + index,
            "op": "remove",
            "character": index.to_string(),
        })));
        assert_eq!(reply, json!({"id": 40 + index, "ok": null}));
    }
    harness.app.update();
    assert_eq!(
        harness
            .app
            .world_mut()
            .query::<&Vrm>()
            .iter(harness.app.world())
            .count(),
        0
    );

    drop(harness);
}
