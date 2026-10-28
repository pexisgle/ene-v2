//! Pointer drag and click handling.

use super::{HitRegions, Output, bounds, desktop_position};
use crate::character::{AvatarState, Character, Notification};
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
    mut actors: Query<(&mut Character, &Transform)>,
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
                && let Ok((character, transform)) = actors.get(entity)
                && character.state == AvatarState::Ready
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
            if let Ok((mut character, _)) = actors.get_mut(entity)
                && character.state == AvatarState::Ready
            {
                character.config.position = Some(
                    ((position - destination.origin) / destination.units_per_logical_pixel
                        + drag.offset)
                        .to_array(),
                );
                character.config.monitor = Some(destination.name.clone());
            }
        }
        if input.button == Some(false) {
            if let Ok((character, _)) = actors.get(entity) {
                if drag.moved {
                    // The client persists the placement the renderer resolved.
                    clicks.write(Notification(json!({
                        "event": "moved",
                        "character": character.id,
                        "monitor": character.config.monitor,
                        "position": character.config.position,
                    })));
                } else if character.state == AvatarState::Ready {
                    clicks.write(Notification(
                        json!({"event":"click", "character":character.id}),
                    ));
                }
            }
            drag.entity = None;
        }
    }
}
