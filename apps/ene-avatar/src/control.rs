use crate::{
    character::{AvatarState, CharacterId, Config, Notification, reload},
    desktop::Output,
};
use bevy::{
    app::{AppExit, MainScheduleOrder},
    ecs::schedule::ScheduleLabel,
    prelude::*,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{BufRead, Read, Write},
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};

impl Config {
    fn validate(&self) -> Result<(), String> {
        for path in std::iter::once(&self.model).chain(self.motion.iter()) {
            bevy::asset::AssetPath::try_parse(path).map_err(|error| error.to_string())?;
        }
        let invalid_size = !(50. ..=200.).contains(&self.size_percent)
            || self
                .position
                .is_some_and(|p| p.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.));
        let invalid_path = !self.model.ends_with(".vrm")
            || self
                .motion
                .as_ref()
                .is_some_and(|path| !path.ends_with(".vrma"));
        match (invalid_size, invalid_path) {
            (true, _) => Err("invalid size or position".into()),
            (_, true) => Err("expected .vrm or .vrma path".into()),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct Request {
    id: u64,
    #[serde(flatten)]
    command: Command,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Add {
        character: String,
        config: Config,
    },
    Set {
        character: String,
        config: Config,
    },
    Get,
    Remove {
        character: String,
    },
    Center {
        character: String,
        monitor: Option<String>,
    },
    Fps {
        value: u32,
    },
    Quit,
}

#[derive(Resource)]
pub struct Control {
    pub output: SyncSender<Value>,
    pub fps: u32,
    #[cfg(target_os = "linux")]
    pub wake_tx: exwlshellev::calloop::channel::Sender<()>,
}

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameEnd;

pub fn run(mut app: App) -> AppExit {
    let done = app
        .world_mut()
        .remove_non_send::<Receiver<std::io::Result<()>>>()
        .expect("control plugin must be installed");
    let exit = app.run();
    match done.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => exit,
        result => {
            error!("renderer output did not finish: {result:?}");
            AppExit::error()
        }
    }
}

fn write_output(mut writer: impl Write, output: Receiver<Value>) -> std::io::Result<()> {
    for message in output {
        writeln!(writer, "{message}")?;
        writer.flush()?;
    }
    Ok(())
}

pub fn setup(app: &mut App) {
    let (input_tx, input) = mpsc::sync_channel(64);
    let (output, output_rx) = mpsc::sync_channel::<Value>(64);
    let (done, output_done) = mpsc::channel();
    #[cfg(target_os = "linux")]
    let (wake_tx, wake_rx) = exwlshellev::calloop::channel::channel();
    #[cfg(target_os = "linux")]
    app.insert_non_send(wake_rx);
    #[cfg(target_os = "linux")]
    let input_wake = wake_tx.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            let result = (&mut stdin).take(65_537).read_line(&mut line);
            let request = match result {
                Ok(0) => Ok(Request {
                    id: 0,
                    command: Command::Quit,
                }),
                Ok(n) if n <= 65_536 => serde_json::from_str(&line).map_err(|e| e.to_string()),
                _ => Err("input line exceeds 64 KiB or could not be read".into()),
            };
            let end = line.len() > 65_536 || line.is_empty();
            if input_tx.send(request).is_err() {
                break;
            }
            #[cfg(target_os = "linux")]
            let _ = input_wake.send(());
            if end {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let result = write_output(std::io::stdout().lock(), output_rx);
        let _ = done.send(result);
    });
    app.insert_non_send(output_done)
        .insert_non_send(input)
        .insert_resource(Control {
            output,
            fps: 30,
            #[cfg(target_os = "linux")]
            wake_tx,
        })
        .add_systems(PreUpdate, requests)
        .add_systems(PostUpdate, publish.after(crate::character::notify_finished));
    app.world_mut()
        .resource_mut::<MainScheduleOrder>()
        .insert_after(Last, FrameEnd);
}

/// Exclusive on purpose: a batch that arrives in one frame is applied to the
/// World immediately, so `add` followed by `get` in the same batch is coherent
/// and no artificial one-request-per-frame throttle is needed.
fn requests(
    world: &mut World,
    avatars: &mut QueryState<(Entity, &CharacterId, &mut Config, &AvatarState)>,
) {
    for _ in 0..64 {
        let request = match world
            .non_send::<Receiver<Result<Request, String>>>()
            .try_recv()
        {
            Ok(request) => request,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                world.write_message(AppExit::Success);
                break;
            }
        };
        let id = request.as_ref().map(|request| request.id).unwrap_or(0);
        let result = request.and_then(|request| {
            let add = matches!(&request.command, Command::Add { .. });
            let target = match &request.command {
                Command::Add { character, .. }
                | Command::Set { character, .. }
                | Command::Remove { character }
                | Command::Center { character, .. } => avatars
                    .iter_mut(world)
                    .find(|(_, id, ..)| id.0 == *character),
                _ => None,
            };
            match request.command {
                Command::Add { character, config } | Command::Set { character, config } => {
                    config.validate()?;
                    let entity = match (add, target) {
                        (true, None) if !character.is_empty() && character.len() <= 128 => {
                            world.spawn((CharacterId(character), config)).id()
                        }
                        (true, _) => return Err("invalid or duplicate character id".into()),
                        (false, None) => return Err("unknown character".into()),
                        (false, Some((entity, _, mut old, state))) => {
                            let rebuild = matches!(state, AvatarState::Error(_))
                                || old.model != config.model
                                || old.motion != config.motion;
                            old.set_if_neq(config);
                            if !rebuild {
                                return Ok(Value::Null);
                            }
                            entity
                        }
                    };
                    world
                        .run_system_cached_with(reload, entity)
                        .map_err(|error| error.to_string())?;
                }
                Command::Get => {
                    return world
                        .run_system_cached(snapshot)
                        .map_err(|error| error.to_string());
                }
                Command::Remove { .. } => {
                    let (entity, ..) = target.ok_or("unknown character")?;
                    world.despawn(entity);
                }
                Command::Center { monitor, .. } => {
                    let (_, _, mut config, _) = target.ok_or("unknown character")?;
                    config.monitor = monitor;
                    config.position = None;
                }
                Command::Fps { value } => {
                    if value > 1000 {
                        return Err("FPS must be 0..=1000".into());
                    }
                    world.resource_mut::<Control>().fps = value;
                }
                Command::Quit => {
                    world.write_message(AppExit::Success);
                }
            }
            Ok(Value::Null)
        });
        let response = match result {
            Ok(value) => json!({"id":id, "ok":value}),
            Err(error) => json!({"id":id, "error":error}),
        };
        if let Err(error) = world.resource::<Control>().output.try_send(response) {
            error!("renderer output unavailable: {error}");
            world.write_message(AppExit::error());
            return;
        }
    }
}

#[allow(clippy::type_complexity)]
fn snapshot(
    views: Query<(Entity, &CharacterId, &Config, &AvatarState)>,
    outputs: Query<&Output>,
    children: Query<&Children>,
    players: Query<&AnimationPlayer>,
    control: Res<Control>,
) -> Value {
    let mut characters = Vec::new();
    for (entity, id, config, state) in &views {
        let animation_times: Vec<_> = players
            .iter_many(std::iter::once(entity).chain(children.iter_descendants(entity)))
            .flat_map(AnimationPlayer::playing_animations)
            .map(|(_, animation)| animation.seek_time())
            .collect();
        let output = crate::desktop::pick_output(config, outputs.iter());
        let mut resolved = config.clone();
        resolved.monitor = output.map(|output| output.name.clone());
        resolved.position = crate::desktop::resolved_position(config, output).map(|p| p.to_array());
        let mut actor = state.json();
        actor["character"] = json!(id.0);
        actor["config"] = json!(resolved);
        actor["animation_times"] = json!(animation_times);
        characters.push(actor);
    }
    let monitors: Vec<_> = outputs.iter().map(|o| json!({"name":o.name,"origin":o.origin.to_array(),"size":o.size.to_array(),"units_per_logical_pixel":o.units_per_logical_pixel})).collect();
    json!({"characters":characters, "monitors":monitors, "fps":control.fps})
}

/// Flush notifications after command replies, transferring ownership to stdout.
fn publish(
    control: Res<Control>,
    mut events: ResMut<Messages<Notification>>,
    mut exit: MessageWriter<AppExit>,
) {
    for Notification(event) in events.drain() {
        if let Err(error) = control.output.try_send(event) {
            error!("renderer output unavailable: {error}");
            exit.write(AppExit::error());
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> (App, SyncSender<Result<Request, String>>, Receiver<Value>) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AssetPlugin::default()))
            .init_asset::<bevy::gltf::Gltf>()
            .init_asset::<WorldAsset>()
            .init_asset::<AnimationGraph>()
            .init_asset::<AnimationClip>()
            .add_message::<Notification>()
            .add_systems(PreUpdate, requests);
        let (input, receiver) = mpsc::sync_channel(8);
        let (output, output_rx) = mpsc::sync_channel(8);
        app.insert_non_send(receiver).insert_resource(Control {
            output,
            fps: 30,
            #[cfg(target_os = "linux")]
            wake_tx: exwlshellev::calloop::channel::channel().0,
        });
        (app, input, output_rx)
    }

    #[test]
    fn shutdown_drains_replies_and_preserves_the_application_exit_code() {
        for exit in [AppExit::Success, AppExit::error()] {
            let expected = exit.clone();
            let mut app = App::new();
            let (output, receiver) = mpsc::sync_channel(64);
            let (done, completion) = mpsc::channel();
            app.insert_non_send(completion);
            app.set_runner(move |_| {
                for id in 0..17 {
                    output.try_send(json!({"id":id,"ok":null})).unwrap();
                }
                exit
            });
            let writer = std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = write_output(&mut bytes, receiver);
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
    }

    #[test]
    fn shutdown_reports_failed_disconnected_or_stalled_output() {
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
        sender.send(json!({"id":1,"ok":null})).unwrap();
        drop(sender);
        let mut closed = std::io::Cursor::new(&mut [][..]);
        assert!(write_output(&mut closed, receiver).is_err());
    }

    #[test]
    fn invalid_asset_paths_are_rejected_before_changing_characters() {
        let (mut app, input, output) = test_app();
        let original = Config {
            model: "good.vrm".into(),
            ..default()
        };
        let actor = app
            .world_mut()
            .spawn((CharacterId("existing".into()), original.clone()))
            .id();
        for (id, op, character, config) in [
            (1, "add", "new", json!({"model":"://bad.vrm"})),
            (
                2,
                "set",
                "existing",
                json!({"model":"good.vrm","motion":"://bad.vrma"}),
            ),
        ] {
            input
                .send(
                    serde_json::from_value(
                        json!({"id":id,"op":op,"character":character,"config":config}),
                    )
                    .map_err(|error| error.to_string()),
                )
                .unwrap();
        }
        app.update();
        let replies: Vec<_> = output.try_iter().collect();
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().all(|reply| reply["error"].is_string()));
        assert!(app.world().get::<Config>(actor).unwrap() == &original);
        assert_eq!(
            app.world_mut()
                .query::<&CharacterId>()
                .iter(app.world())
                .count(),
            1
        );
    }

    #[test]
    fn full_or_disconnected_output_exits_instead_of_blocking_the_app() {
        for (disconnected, notification) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let (mut app, input, output) = test_app();
            if notification {
                app.add_systems(PostUpdate, publish);
                app.world_mut()
                    .write_message(Notification(json!({"event":"finished", "character":"a"})));
            }
            for _ in 0..8 {
                app.world()
                    .resource::<Control>()
                    .output
                    .try_send(Value::Null)
                    .unwrap();
            }
            if disconnected {
                drop(output);
            }
            if !notification {
                input
                    .send(Ok(Request {
                        id: 1,
                        command: Command::Get,
                    }))
                    .unwrap();
            }
            app.update();
            assert_eq!(app.should_exit(), Some(AppExit::error()));
        }
    }

    #[test]
    fn reject_invalid_settings_and_accept_a_command_without_conversion() {
        let request: Request = serde_json::from_str(
            r#"{"id":1,"op":"add","character":"a","config":{"model":"test.vrm"}}"#,
        )
        .unwrap();
        let Command::Add { mut config, .. } = request.command else {
            panic!()
        };
        assert!(config.validate().is_ok());
        for size in [f32::NAN, f32::INFINITY, 49., 201.] {
            config.size_percent = size;
            assert!(config.validate().is_err());
        }
        config.size_percent = 100.;
        config.position = Some([f32::NAN, 1.]);
        assert!(config.validate().is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"id":1,"op":"fps","value":30,"typo":true}"#)
                .is_err()
        );
    }

    #[test]
    fn requests_apply_commands_immediately() {
        let (mut app, input, output) = test_app();
        app.world_mut().spawn(Output {
            name: "m".into(),
            origin: Vec2::ZERO,
            size: Vec2::splat(100.),
            units_per_logical_pixel: 1.,
        });
        macro_rules! send {
            ($request:literal) => {{
                input
                    .send(serde_json::from_str($request).map_err(|error| error.to_string()))
                    .unwrap();
                app.update();
                output.recv().unwrap()
            }};
        }
        assert_eq!(
            send!(r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#)["ok"],
            Value::Null
        );
        let entity = {
            let mut query = app.world_mut().query::<(Entity, &CharacterId)>();
            query.single(app.world()).unwrap().0
        };
        assert_eq!(app.world().get::<CharacterId>(entity).unwrap().0, "a");
        let response = send!(r#"{"id":2,"op":"get"}"#);
        assert_eq!(response["ok"]["characters"][0]["status"], "loading");
        assert_eq!(response["ok"]["monitors"][0]["name"], "m");
        assert_eq!(response["ok"]["characters"][0]["config"]["monitor"], "m");
        assert_eq!(
            response["ok"]["characters"][0]["config"]["position"],
            json!([50., 80.])
        );
        assert_eq!(app.world().get::<Config>(entity).unwrap().position, None);
        assert_eq!(app.world().get::<Config>(entity).unwrap().monitor, None);
        assert_eq!(
            send!(
                r#"{"id":3,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}"#
            )["ok"],
            Value::Null
        );
        assert!(!app.world().get::<Config>(entity).unwrap().playing);
        send!(r#"{"id":4,"op":"remove","character":"a"}"#);
        assert!(app.world().get::<CharacterId>(entity).is_none());
    }

    #[test]
    fn requests_apply_a_whole_batch_in_one_frame() {
        let (mut app, input, output) = test_app();
        for line in [
            r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#,
            r#"{"id":2,"op":"set","character":"a","config":{"model":"a.vrm","playing":false}}"#,
            r#"{"id":3,"op":"get"}"#,
        ] {
            input
                .send(serde_json::from_str(line).map_err(|error| error.to_string()))
                .unwrap();
        }
        app.update();
        assert_eq!(output.recv().unwrap(), json!({"id": 1, "ok": Value::Null}));
        assert_eq!(output.recv().unwrap(), json!({"id": 2, "ok": Value::Null}));
        let get = output.recv().unwrap();
        let characters = get["ok"]["characters"].as_array().unwrap();
        assert_eq!(characters.len(), 1);
        assert_eq!(
            characters[0]["config"],
            json!({
                "model": "a.vrm", "motion": null, "playing": false, "looping": true,
                "monitor": null, "position": null, "size_percent": 100.,
            })
        );
    }

    #[test]
    fn batch_replies_precede_loading_notifications() {
        let (mut app, input, output) = test_app();
        app.add_systems(PostUpdate, publish);
        for line in [
            r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#,
            r#"{"id":2,"op":"get"}"#,
        ] {
            input
                .send(serde_json::from_str(line).map_err(|error| error.to_string()))
                .unwrap();
        }
        app.update();
        let messages: Vec<_> = output.try_iter().collect();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0], json!({"id":1,"ok":null}));
        assert_eq!(messages[1]["id"], 2);
        assert_eq!(
            messages[2],
            json!({"event":"state","character":"a","status":"loading","error":null})
        );
    }

    #[test]
    fn cached_query_handles_rejections_centering_and_readding_in_one_batch() {
        let (mut app, input, output) = test_app();
        for line in [
            r#"{"id":1,"op":"set","character":"a","config":{"model":"a.vrm"}}"#,
            r#"{"id":2,"op":"add","character":"a","config":{"model":"a.vrm","playing":false,"position":[3,4]}}"#,
            r#"{"id":3,"op":"add","character":"a","config":{"model":"b.vrm"}}"#,
            r#"{"id":4,"op":"center","character":"a","monitor":"m"}"#,
            r#"{"id":5,"op":"get"}"#,
            r#"{"id":6,"op":"remove","character":"a"}"#,
            r#"{"id":7,"op":"add","character":"a","config":{"model":"b.vrm","position":[5,6]}}"#,
            r#"{"id":8,"op":"get"}"#,
        ] {
            input
                .send(serde_json::from_str(line).map_err(|error| error.to_string()))
                .unwrap();
        }
        app.update();
        let replies: Vec<_> = output.try_iter().collect();
        assert_eq!(replies.len(), 8);
        assert_eq!(replies[0], json!({"id":1,"error":"unknown character"}));
        assert_eq!(
            replies[2],
            json!({"id":3,"error":"invalid or duplicate character id"})
        );
        let first = &replies[4]["ok"]["characters"][0]["config"];
        assert_eq!(first["model"], "a.vrm");
        assert_eq!(first["playing"], false);
        assert_eq!(first["position"], Value::Null);
        let final_avatars = replies[7]["ok"]["characters"].as_array().unwrap();
        assert_eq!(final_avatars.len(), 1);
        assert_eq!(final_avatars[0]["config"]["model"], "b.vrm");
        assert_eq!(final_avatars[0]["config"]["position"], json!([5., 6.]));
    }

    #[test]
    fn settings_only_reload_changed_sources_and_retry_errors() {
        #[derive(Resource, Default)]
        struct Reloads(usize);
        let (mut app, input, output) = test_app();
        app.init_resource::<Reloads>().add_observer(
            |_: On<Insert, crate::character::ModelAssets>, mut count: ResMut<Reloads>| count.0 += 1,
        );
        for (index, (model, motion, playing, reloads)) in [
            ("a.vrm", None, true, 1),
            ("a.vrm", None, false, 1),
            ("a.vrm", Some("a.vrma"), false, 2),
            ("a.vrm", None, false, 3),
            ("b.vrm", None, false, 4),
        ]
        .into_iter()
        .enumerate()
        {
            input
                .send(
                    serde_json::from_value(json!({
                        "id":index,"op":if index == 0 { "add" } else { "set" },
                        "character":"a","config":{"model":model,"motion":motion,"playing":playing}
                    }))
                    .map_err(|error| error.to_string()),
                )
                .unwrap();
            app.update();
            assert_eq!(output.recv().unwrap()["ok"], Value::Null);
            assert_eq!(app.world().resource::<Reloads>().0, reloads);
        }
        let entity = app
            .world_mut()
            .query_filtered::<Entity, With<CharacterId>>()
            .single(app.world())
            .unwrap();
        let changed = app
            .world()
            .entity(entity)
            .get_ref::<Config>()
            .unwrap()
            .last_changed();
        input
            .send(Ok(Request {
                id: 5,
                command: Command::Set {
                    character: "a".into(),
                    config: app.world().get::<Config>(entity).unwrap().clone(),
                },
            }))
            .unwrap();
        app.update();
        assert_eq!(output.recv().unwrap(), json!({"id":5,"ok":null}));
        assert_eq!(app.world().resource::<Reloads>().0, 4);
        assert_eq!(
            app.world()
                .entity(entity)
                .get_ref::<Config>()
                .unwrap()
                .last_changed(),
            changed
        );
        app.world_mut()
            .entity_mut(entity)
            .insert(AvatarState::Error("broken".into()));
        let config = app.world().get::<Config>(entity).unwrap().clone();
        input
            .send(Ok(Request {
                id: 6,
                command: Command::Set {
                    character: "a".into(),
                    config,
                },
            }))
            .unwrap();
        app.update();
        assert_eq!(output.recv().unwrap()["ok"], Value::Null);
        assert_eq!(app.world().resource::<Reloads>().0, 5);
        assert_eq!(
            app.world().get::<AvatarState>(entity),
            Some(&AvatarState::Loading)
        );
    }

    #[test]
    fn get_reads_animation_times_without_spring_diagnostics() {
        let (mut app, input, output) = test_app();
        input
            .send(
                serde_json::from_str(
                    r#"{"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}"#,
                )
                .map_err(|error| error.to_string()),
            )
            .unwrap();
        app.update();
        let _ = output.recv().unwrap();
        let entity = {
            let mut query = app.world_mut().query::<(Entity, &CharacterId)>();
            query.single(app.world()).unwrap().0
        };
        let mut player = AnimationPlayer::default();
        player
            .play(AnimationNodeIndex::new(0))
            .repeat()
            .seek_to(1.25);
        app.world_mut().spawn((player, ChildOf(entity)));
        input
            .send(Ok(Request {
                id: 2,
                command: Command::Get,
            }))
            .unwrap();
        app.update();
        let get = output.recv().unwrap();
        let actor = &get["ok"]["characters"][0];
        assert_eq!(actor["animation_times"], json!([1.25]));
        assert!(actor.get("spring_paused").is_none());
    }

    #[test]
    fn publish_serializes_state_finished_and_click() {
        let (mut app, _input, output) = test_app();
        app.add_systems(PostUpdate, publish);
        for mut value in [
            AvatarState::Ready.json(),
            json!({"event":"finished"}),
            json!({"event":"click"}),
            AvatarState::Error("broken\"model".into()).json(),
        ] {
            if value.get("status").is_some() {
                value["event"] = "state".into();
            }
            value["character"] = "a".into();
            app.world_mut().write_message(Notification(value));
        }
        app.update();
        let values: Vec<_> = output.try_iter().collect();
        assert_eq!(values.len(), 4);
        assert_eq!(
            values[0],
            json!({"event":"state","character":"a","status":"ready","error":null})
        );
        assert_eq!(values[1]["event"], "finished");
        assert_eq!(values[2]["event"], "click");
        assert_eq!(
            values[3],
            json!({"event":"state","character":"a","status":"error","error":"broken\"model"})
        );
    }
}
