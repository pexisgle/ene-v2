//! Applying commands to a single renderer character.

use super::ControlError;
use crate::{
    character::{
        AvatarState, Character, Config, cancel_motion, queue_motion, reload, validate_config,
    },
    desktop::Layout,
};
use bevy::prelude::*;

pub(super) fn apply_update(
    world: &mut World,
    target: Option<Entity>,
    character: String,
    mut config: Config,
) -> Result<(), ControlError> {
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let previous = world
        .get::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    let placement = (config.monitor.clone(), config.position);
    config.monitor = config.monitor.or_else(|| previous.config.monitor.clone());
    config.position = config.position.or(previous.config.position);
    apply_add_or_set(world, false, target, character, config)?;
    if placement.0.is_some() || placement.1.is_some() {
        let _recorded = world.try_resource_scope(|world, mut layout: Mut<Layout>| {
            if let Some(avatar) = world.get::<Character>(entity) {
                layout.rebind(&avatar.id, placement, &avatar.config);
            }
        });
    }
    Ok(())
}

pub(super) fn apply_add_or_set(
    world: &mut World,
    add: bool,
    target: Option<Entity>,
    character: String,
    mut config: Config,
) -> Result<(), ControlError> {
    validate_config(&config)?;
    let entity = match (add, target) {
        (true, None) if !character.is_empty() && character.len() <= 128 => {
            if let Some(mut layout) = world.get_resource_mut::<Layout>() {
                layout.restore(&character, &mut config);
            }
            world.spawn(Character::new(character, config)).id()
        }
        (true, _) => return Err(ControlError::InvalidCharacterId),
        (false, None) => return Err(ControlError::UnknownCharacter),
        (false, Some(entity)) => {
            let mut old = world
                .get_mut::<Character>(entity)
                .ok_or(ControlError::UnknownCharacter)?;
            let changed_motion = old.config.motion != config.motion;
            let rebuild = matches!(old.state, AvatarState::Error(_))
                || old.config.model != config.model
                || (old.state == AvatarState::Loading && changed_motion);
            if !rebuild {
                if changed_motion {
                    let path = std::mem::replace(&mut config.motion, old.config.motion.clone());
                    let looping = std::mem::replace(&mut config.looping, old.config.looping);
                    old.config = config;
                    queue_motion(world, entity, path, looping, None);
                } else {
                    old.config = config;
                    cancel_motion(world, entity);
                }
                return Ok(());
            }
            old.config = config;
            entity
        }
    };
    reload(world, entity, None);
    Ok(())
}

pub(super) fn apply_motion(
    world: &mut World,
    target: Option<Entity>,
    motion: Option<String>,
    looping: bool,
    request: Option<u64>,
) -> Result<(), ControlError> {
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
    if matches!(old.state, AvatarState::Loading | AvatarState::Error(_)) {
        old.config = config;
        reload(world, entity, request);
    } else {
        queue_motion(world, entity, config.motion, looping, request);
    }
    Ok(())
}

pub(super) fn apply_expression(
    world: &mut World,
    target: Option<Entity>,
    name: Option<String>,
) -> Result<(), ControlError> {
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    character.expression = match name {
        Some(name) if !name.trim().is_empty() && name.len() <= 64 => Some(name),
        Some(_) => return Err(ControlError::InvalidExpressionName),
        None => None,
    };
    Ok(())
}

pub(super) fn apply_lip_sync(
    world: &mut World,
    target: Option<Entity>,
    rms: f32,
) -> Result<(), ControlError> {
    if !rms.is_finite() {
        return Err(ControlError::InvalidLipSync);
    }
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    character.lip = rms.clamp(0., 1.);
    Ok(())
}

pub(super) fn apply_look_at(
    world: &mut World,
    target: Option<Entity>,
    yaw_deg: Option<f32>,
    pitch_deg: Option<f32>,
) -> Result<(), ControlError> {
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
    Ok(())
}

pub(super) fn apply_position(
    world: &mut World,
    target: Option<Entity>,
    monitor: Option<String>,
    position: Option<[f32; 2]>,
) -> Result<(), ControlError> {
    if position.is_some_and(|point| point.iter().any(|v| !v.is_finite() || v.abs() > 1_000_000.)) {
        return Err(ControlError::InvalidPlace);
    }
    let entity = target.ok_or(ControlError::UnknownCharacter)?;
    let mut character = world
        .get_mut::<Character>(entity)
        .ok_or(ControlError::UnknownCharacter)?;
    if position.is_none() || monitor.is_some() {
        character.config.monitor = monitor;
    }
    character.config.position = position;
    let _recorded = world.try_resource_scope(|world, mut layout: Mut<Layout>| {
        if let Some(avatar) = world.get::<Character>(entity) {
            layout.moved(&avatar.id, &avatar.config);
        }
    });
    Ok(())
}
