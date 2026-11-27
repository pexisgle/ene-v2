//! Projects the live renderer into its typed IPC view.
use super::Control;
use crate::{character::Character, desktop::Output};
use bevy::prelude::*;
use ene_avatar_ipc::{CharacterSnapshot, MonitorSnapshot, RendererSnapshot};

pub(super) fn snapshot(
    views: Query<&Character>,
    outputs: Query<&Output>,
    players: Query<&AnimationPlayer>,
    control: Res<Control>,
) -> RendererSnapshot {
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
        let (status, error) = character.state.status();
        characters.push(CharacterSnapshot {
            character: character.id.clone(),
            status,
            error,
            config: resolved,
            expression: character.expression.clone(),
            lip: character.lip,
            lip_level: character.lip_level,
            expression_weight: character.expression_weight,
            animation_times,
        });
    }
    let monitors = outputs
        .iter()
        .map(|output| MonitorSnapshot {
            name: output.name.clone(),
            origin: output.origin.to_array(),
            size: output.size.to_array(),
            units_per_logical_pixel: output.units_per_logical_pixel,
        })
        .collect();
    RendererSnapshot {
        characters,
        monitors,
        fps: control.into_inner().fps,
    }
}
