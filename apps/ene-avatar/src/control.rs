use crate::character::{AvatarState, Character, Config, Notification, reload};
use crate::desktop::Output;
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

/// Reads one bounded JSON line. The flag marks EOF, an overlong line, or an unreadable stream.
fn read_request(input: &mut impl BufRead) -> (Result<Request, String>, bool) {
    let mut line = String::new();
    match (&mut *input).take(65_537).read_line(&mut line) {
        Ok(0) => (
            Ok(Request {
                id: 0,
                command: Command::Quit {},
            }),
            true,
        ),
        Ok(n) if n <= 65_536 => (
            serde_json::from_str(&line).map_err(|error| error.to_string()),
            false,
        ),
        Ok(_) => (Err("input line exceeds 64 KiB".into()), true),
        Err(error) => (Err(error.to_string()), true),
    }
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
            let (request, end) = read_request(&mut stdin);
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
            fps: 60,
            #[cfg(target_os = "linux")]
            wake_tx,
        })
        .add_systems(PreUpdate, requests)
        .add_systems(PostUpdate, publish.after(crate::character::notify_finished));
    app.world_mut()
        .resource_mut::<MainScheduleOrder>()
        .insert_after(Last, FrameEnd);
}

#[derive(Deserialize)]
struct Request {
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
    Get {},
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
    Quit {},
}

/// Exclusive on purpose: a batch that arrives in one frame is applied to the
/// World immediately, so `add` followed by `get` in the same batch is coherent
/// and no artificial one-request-per-frame throttle is needed.
fn requests(world: &mut World, avatars: &mut QueryState<(Entity, &mut Character)>) {
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
                | Command::Center { character, .. } => {
                    avatars.iter_mut(world).find(|(_, c)| c.id == *character)
                }
                _ => None,
            };
            match request.command {
                Command::Add { character, config } | Command::Set { character, config } => {
                    config.validate()?;
                    let entity = match (add, target) {
                        (true, None) if !character.is_empty() && character.len() <= 128 => {
                            world.spawn(Character::new(character, config)).id()
                        }
                        (true, _) => return Err("invalid or duplicate character id".into()),
                        (false, None) => return Err("unknown character".into()),
                        (false, Some((entity, mut old))) => {
                            let rebuild = matches!(old.state, AvatarState::Error(_))
                                || old.config.model != config.model
                                || old.config.motion != config.motion;
                            old.config = config;
                            if !rebuild {
                                return Ok(Value::Null);
                            }
                            entity
                        }
                    };
                    reload(world, entity);
                }
                Command::Get {} => {
                    return world
                        .run_system_cached(snapshot)
                        .map_err(|error| error.to_string());
                }
                Command::Remove { .. } => {
                    let (entity, ..) = target.ok_or("unknown character")?;
                    world.despawn(entity);
                }
                Command::Center { monitor, .. } => {
                    let (_, mut character) = target.ok_or("unknown character")?;
                    character.config.monitor = monitor;
                    character.config.position = None;
                }
                Command::Fps { value } => {
                    if value > 1000 {
                        return Err("FPS must be 0..=1000".into());
                    }
                    world.resource_mut::<Control>().fps = value;
                }
                Command::Quit {} => {
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

fn snapshot(
    views: Query<&Character>,
    outputs: Query<&Output>,
    players: Query<&AnimationPlayer>,
    control: Res<Control>,
) -> Value {
    let mut characters = Vec::new();
    for character in &views {
        let config = &character.config;
        let animation_times: Vec<_> = character
            .root
            .and_then(|root| players.get(root).ok())
            .into_iter()
            .flat_map(AnimationPlayer::playing_animations)
            .map(|(_, animation)| animation.seek_time())
            .collect();
        let output = crate::desktop::pick_output(config, outputs.iter());
        let mut resolved = config.clone();
        resolved.monitor = output.map(|output| output.name.clone());
        resolved.position = crate::desktop::resolved_position(config, output).map(|p| p.to_array());
        let mut actor = character.state.json();
        actor["character"] = json!(character.id);
        actor["config"] = json!(resolved);
        actor["animation_times"] = json!(animation_times);
        characters.push(actor);
    }
    let monitors: Vec<_> = outputs.iter().map(|o| json!({"name":o.name,"origin":o.origin.to_array(),"size":o.size.to_array(),"units_per_logical_pixel":o.units_per_logical_pixel})).collect();
    json!({"characters":characters, "monitors":monitors, "fps":control.fps})
}

fn write_output(mut writer: impl Write, output: Receiver<Value>) -> std::io::Result<()> {
    for message in output {
        writeln!(writer, "{message}")?;
        writer.flush()?;
    }
    Ok(())
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
                unapproved_path_mode: bevy::asset::UnapprovedPathMode::Deny,
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
                fps: 60,
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
}
