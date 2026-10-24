//! Command execution and state snapshots.

use super::{
    Control,
    protocol::{Command, Request},
};
use crate::{
    character::{AvatarState, CharacterId, Config, reload},
    desktop::Output,
};
use bevy::{app::AppExit, prelude::*};
use serde_json::{Value, json};
use std::sync::mpsc::{self, Receiver};

/// Exclusive on purpose: a batch that arrives in one frame is applied to the
/// World immediately, so `add` followed by `get` in the same batch is coherent
/// and no artificial one-request-per-frame throttle is needed.
pub(super) fn requests(
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
