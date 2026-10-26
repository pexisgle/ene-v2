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

#[cfg(test)]
#[path = "control/tests.rs"]
mod tests;

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
