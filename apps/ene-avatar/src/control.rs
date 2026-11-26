use crate::character::{
    AvatarState, Character, Config, ConfigError, Notification, queue_motion, reload,
    validate_config,
};
use crate::desktop::Output;
use bevy::{
    app::{AppExit, MainScheduleOrder},
    ecs::schedule::ScheduleLabel,
    prelude::*,
};
use ene_avatar_ipc::{Command, MAX_LINE, Request};
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
    let Some(done) = app
        .world_mut()
        .remove_non_send::<Receiver<std::io::Result<()>>>()
    else {
        error!("control plugin must be installed");
        return AppExit::error();
    };
    let exit = app.run();
    match done.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => exit,
        result => {
            error!("renderer output did not finish: {result:?}");
            AppExit::error()
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ControlError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("input line exceeds 64 KiB")]
    InputTooLong,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    System(String),
    #[error("unknown character")]
    UnknownCharacter,
    #[error("FPS must be 0..=1000")]
    InvalidFps,
    #[error("invalid or duplicate character id")]
    InvalidCharacterId,
    #[error("invalid expression name")]
    InvalidExpressionName,
    #[error("lip_sync requires a finite amplitude")]
    InvalidLipSync,
    #[error("look_at requires finite angles within yaw [-90, 90] and pitch [-60, 60]")]
    InvalidLookAt,
    #[error("place requires finite logical coordinates")]
    InvalidPlace,
}

/// Reads one bounded JSON line. The flag marks EOF, an overlong line, or an unreadable stream.
fn read_request(input: &mut impl BufRead) -> (Result<Request, ControlError>, bool) {
    let mut line = String::new();
    match (&mut *input)
        .take((MAX_LINE + 1) as u64)
        .read_line(&mut line)
    {
        Ok(0) => (
            Ok(Request {
                id: None,
                command: Command::Quit {},
            }),
            true,
        ),
        Ok(n) if n <= MAX_LINE => (
            serde_json::from_str(&line).map_err(ControlError::from),
            false,
        ),
        Ok(_) => (Err(ControlError::InputTooLong), true),
        Err(error) => (Err(ControlError::Io(error)), true),
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
            let _discarded = input_wake.send(());
            if end {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let result = write_output(std::io::stdout().lock(), output_rx);
        // The receiver only closes when the app is already shutting down, so a
        // failed send has nobody left to report to.
        let _delivered = done.send(result);
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

/// Exclusive on purpose: a batch that arrives in one frame is applied to the
/// World immediately, so `add` followed by `get` in the same batch is coherent
/// and no artificial one-request-per-frame throttle is needed.
fn requests(world: &mut World, avatars: &mut QueryState<(Entity, &mut Character)>) {
    for _ in 0..64 {
        let request = match world
            .non_send::<Receiver<Result<Request, ControlError>>>()
            .try_recv()
        {
            Ok(request) => request,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                world.write_message(AppExit::Success);
                break;
            }
        };
        let id = request.as_ref().ok().and_then(|request| request.id);
        let result = request.and_then(|request| apply_command(world, avatars, request.command));
        // A one-way notification has no id and must not answer, or the client
        // would have to drain replies it never asked for.
        if !send_command_reply(world, id, result) {
            return;
        }
    }
}

fn find_target_entity(
    world: &mut World,
    avatars: &mut QueryState<(Entity, &mut Character)>,
    command: &Command,
) -> Option<Entity> {
    let target_name = match command {
        Command::Add { character, .. }
        | Command::Set { character, .. }
        | Command::Motion { character, .. }
        | Command::Remove { character }
        | Command::Center { character, .. }
        | Command::Expression { character, .. }
        | Command::LipSync { character, .. }
        | Command::LookAt { character, .. }
        | Command::Place { character, .. } => character.as_str(),
        _ => return None,
    };
    avatars
        .iter(world)
        .find(|(_, c)| c.id == target_name)
        .map(|(entity, _)| entity)
}

fn apply_command(
    world: &mut World,
    avatars: &mut QueryState<(Entity, &mut Character)>,
    command: Command,
) -> Result<Value, ControlError> {
    let target = find_target_entity(world, avatars, &command);
    match command {
        Command::Add { character, config } => {
            apply_add_or_set(world, true, target, character, config)
        }
        Command::Set { character, config } => {
            apply_add_or_set(world, false, target, character, config)
        }
        Command::Motion {
            motion, looping, ..
        } => apply_motion(world, target, motion, looping),
        Command::Get {} => world
            .run_system_cached(snapshot)
            .map_err(|error| ControlError::System(error.to_string())),
        Command::Remove { .. } => {
            let entity = target.ok_or(ControlError::UnknownCharacter)?;
            world.despawn(entity);
            Ok(Value::Null)
        }
        Command::Center { monitor, .. } => {
            let entity = target.ok_or(ControlError::UnknownCharacter)?;
            let mut character = world
                .get_mut::<Character>(entity)
                .ok_or(ControlError::UnknownCharacter)?;
            character.config.monitor = monitor;
            character.config.position = None;
            Ok(Value::Null)
        }
        Command::Fps { value } => {
            if value > 1000 {
                return Err(ControlError::InvalidFps);
            }
            world.resource_mut::<Control>().fps = value;
            Ok(Value::Null)
        }
        Command::Expression { name, .. } => apply_expression(world, target, name),
        Command::LipSync { rms, .. } => apply_lip_sync(world, target, rms),
        Command::LookAt {
            yaw_deg, pitch_deg, ..
        } => apply_look_at(world, target, yaw_deg, pitch_deg),
        Command::Place {
            monitor, position, ..
        } => apply_place(world, target, monitor, position),
        Command::Quit {} => {
            world.write_message(AppExit::Success);
            Ok(Value::Null)
        }
    }
}

fn apply_add_or_set(
    world: &mut World,
    add: bool,
    target: Option<Entity>,
    character: String,
    config: Config,
) -> Result<Value, ControlError> {
    validate_config(&config)?;
    let entity = match (add, target) {
        (true, None) if !character.is_empty() && character.len() <= 128 => {
            world.spawn(Character::new(character, config)).id()
        }
        (true, _) => return Err(ControlError::InvalidCharacterId),
        (false, None) => return Err(ControlError::UnknownCharacter),
        (false, Some(entity)) => {
            let mut old = world
                .get_mut::<Character>(entity)
                .ok_or(ControlError::UnknownCharacter)?;
            let rebuild =
                matches!(old.state, AvatarState::Error(_)) || old.config.model != config.model;
            let previous = old.config.motion.clone();
            old.config = config;
            if !rebuild {
                // The model is staying: fade to the new clip
                // instead of despawning and reloading it.
                if previous != old.config.motion {
                    queue_motion(world, entity);
                }
                return Ok(Value::Null);
            }
            entity
        }
    };
    reload(world, entity);
    Ok(Value::Null)
}

fn apply_motion(
    world: &mut World,
    target: Option<Entity>,
    motion: Option<String>,
    looping: bool,
) -> Result<Value, ControlError> {
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut old = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    let config = Config {
        motion,
        looping,
        ..old.config.clone()
    };
    validate_config(&config)?;
    old.config = config;
    let should_reload = matches!(old.state, AvatarState::Error(_));
    if should_reload {
        reload(world, entity);
    } else {
        queue_motion(world, entity);
    }
    Ok(Value::Null)
}

fn apply_expression(
    world: &mut World,
    target: Option<Entity>,
    name: Option<String>,
) -> Result<Value, ControlError> {
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    character.expression = match name {
        Some(name) if !name.trim().is_empty() && name.len() <= 64 => Some(name),
        Some(_) => return Err(ControlError::InvalidExpressionName),
        None => None,
    };
    Ok(Value::Null)
}

fn apply_lip_sync(
    world: &mut World,
    target: Option<Entity>,
    rms: f32,
) -> Result<Value, ControlError> {
    if !rms.is_finite() {
        return Err(ControlError::InvalidLipSync);
    }
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    character.lip = rms.clamp(0., 1.);
    Ok(Value::Null)
}

fn apply_look_at(
    world: &mut World,
    target: Option<Entity>,
    yaw_deg: Option<f32>,
    pitch_deg: Option<f32>,
) -> Result<Value, ControlError> {
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    match (yaw_deg, pitch_deg) {
        (None, None) => character.gaze = None,
        (yaw, pitch) => {
            let yaw = yaw.unwrap_or(0.);
            let pitch = pitch.unwrap_or(0.);
            if !yaw.is_finite() || !pitch.is_finite() || yaw.abs() > 90. || pitch.abs() > 60. {
                return Err(ControlError::InvalidLookAt);
            }
            character.gaze = Some((yaw, pitch));
        }
    }
    Ok(Value::Null)
}

fn apply_place(
    world: &mut World,
    target: Option<Entity>,
    monitor: Option<String>,
    position: [f32; 2],
) -> Result<Value, ControlError> {
    if position
        .iter()
        .any(|v| !v.is_finite() || v.abs() > 1_000_000.)
    {
        return Err(ControlError::InvalidPlace);
    }
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    if monitor.is_some() {
        character.config.monitor = monitor;
    }
    character.config.position = Some(position);
    Ok(Value::Null)
}

fn send_command_reply(
    world: &mut World,
    id: Option<u64>,
    result: Result<Value, ControlError>,
) -> bool {
    match id {
        Some(id) => {
            let response = match result {
                Ok(value) => json!({"id":id, "ok":value}),
                Err(error) => json!({"id":id, "error":error.to_string()}),
            };
            if let Err(error) = world.resource::<Control>().output.try_send(response) {
                error!("renderer output unavailable: {error}");
                world.write_message(AppExit::error());
                return false;
            }
        }
        None => {
            if let Err(error) = result {
                warn!("dropped a one-way command: {error}");
            }
        }
    }
    true
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
        actor["expression"] = json!(character.expression);
        actor["lip"] = json!(character.lip);
        actor["lip_level"] = json!(character.lip_level);
        actor["expression_weight"] = json!(character.expression_weight);
        actor["animation_times"] = json!(animation_times);
        characters.push(actor);
    }
    let monitors: Vec<_> = outputs.iter().map(|o| json!({"name":o.name,"origin":o.origin.to_array(),"size":o.size.to_array(),"units_per_logical_pixel":o.units_per_logical_pixel})).collect();
    json!({"characters":characters, "monitors":monitors, "fps":control.into_inner().fps})
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
    let output = &control.into_inner().output;
    for Notification(event) in events.drain() {
        if let Err(error) = output.try_send(event) {
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
        input: SyncSender<Result<Request, ControlError>>,
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
                    .send(serde_json::from_value(command.clone()).map_err(ControlError::from))
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
                    id: Some(1),
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
    fn one_way_lip_sync_and_expression_are_visible_in_the_snapshot() {
        let mut h = Harness::new();
        let messages = h.batch(&[
            json!({"id":1,"op":"add","character":"a","config":{"model":"a.vrm"}}),
            json!({"op":"expression","character":"a","name":"happy"}),
            json!({"op":"lip_sync","character":"a","rms":0.42}),
            json!({"id":2,"op":"get"}),
        ]);
        // The two one-way lines must not answer. A state notification may also
        // arrive, so only the id-carrying messages are counted.
        let replies: Vec<_> = messages
            .iter()
            .filter(|message| message.get("id").is_some())
            .collect();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0]["id"], 1);
        let actor = &replies[1]["ok"]["characters"][0];
        assert_eq!(actor["expression"], "happy");
        let lip = actor["lip"].as_f64().unwrap();
        assert!((lip - 0.42).abs() < 1e-6, "{lip}");

        let messages = h.batch(&[
            json!({"id":3,"op":"expression","character":"a","name":null}),
            json!({"op":"lip_sync","character":"a","rms":4.5}),
            json!({"id":4,"op":"get"}),
        ]);
        let replies: Vec<_> = messages
            .iter()
            .filter(|message| message.get("id").is_some())
            .collect();
        assert_eq!(replies[0], &json!({"id":3,"ok":null}));
        let actor = &replies[1]["ok"]["characters"][0];
        assert_eq!(actor["expression"], Value::Null);
        // The renderer clamps what a client sends.
        assert_eq!(actor["lip"].as_f64(), Some(1.0));
    }
}
