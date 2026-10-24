//! Pointer drag and click handling.

use super::hit_regions::HitRegions;
use super::output::{Output, bounds, desktop_position};
use crate::character::{AvatarState, CharacterId, Config, Notification};
use bevy::prelude::*;
use serde_json::json;

#[derive(Resource, Default)]
pub struct Drag {
    pub entity: Option<Entity>,
    /// Desktop position where the press happened.
    pub(super) start: Vec2,
    /// Pointer-to-foot offset in logical pixels, unchanged across monitor DPIs.
    pub(super) offset: Vec2,
    pub(super) moved: bool,
}

#[derive(Message)]
pub(crate) struct PointerInput {
    pub output: Entity,
    pub local: Vec2,
    pub button: Option<bool>,
}

pub(crate) fn pointer(
    mut messages: MessageReader<PointerInput>,
    outputs: Query<&Output>,
    regions: Query<&HitRegions>,
    mut actors: Query<(&CharacterId, &mut Config, &Transform, &AvatarState)>,
    mut drag: ResMut<Drag>,
    mut clicks: MessageWriter<Notification>,
) {
    for input in messages.read() {
        let Ok(screen) = outputs.get(input.output) else {
            continue;
        };
        let position = desktop_position(screen, input.local);
        if input.button == Some(true) {
            let hit = regions.get(input.output).ok().and_then(|regions| {
                regions
                    .0
                    .iter()
                    .filter(|(_, rect)| rect.contains(input.local))
                    .max_by_key(|(entity, _)| entity.to_bits())
                    .map(|(id, _)| *id)
            });
            if let Some(entity) = hit
                && let Ok((_, _, transform, state)) = actors.get(entity)
                && *state == AvatarState::Ready
            {
                let anchor = Vec2::new(transform.translation.x, -transform.translation.y);
                *drag = Drag {
                    entity: Some(entity),
                    start: position,
                    offset: (anchor - position) / screen.units_per_logical_pixel,
                    moved: false,
                };
            }
        }
        let Some(entity) = drag.entity else {
            continue;
        };
        drag.moved |= (position - drag.start).length() / screen.units_per_logical_pixel > 4.;
        if drag.moved {
            let destination = outputs
                .iter()
                .find(|output| bounds(output).contains(position))
                .unwrap_or(screen);
            if let Ok((_, mut placement, _, state)) = actors.get_mut(entity)
                && *state == AvatarState::Ready
            {
                placement.position = Some(
                    ((position - destination.origin) / destination.units_per_logical_pixel
                        + drag.offset)
                        .to_array(),
                );
                placement.monitor = Some(destination.name.clone());
            }
        }
        if input.button == Some(false) {
            if !drag.moved
                && let Ok((id, _, _, state)) = actors.get(entity)
                && *state == AvatarState::Ready
            {
                clicks.write(Notification(json!({"event":"click", "character":id.0})));
            }
            drag.entity = None;
        }
    }
}
