//! Avatar placement derived from configuration and outputs.

use super::output::{Output, desktop_position, pick_output, resolved_position};
use crate::character::{CharacterId, Config};
use bevy::prelude::*;

/// Derives placement and stacking depth; unchanged transforms stay untouched.
#[allow(clippy::type_complexity)]
pub(super) fn place_avatars(
    outputs: Query<&Output>,
    mut avatars: Query<(Entity, &Config, &mut Transform), With<CharacterId>>,
) {
    for (index, (_, placement, mut transform)) in avatars
        .iter_mut()
        .sort_unstable_by_key::<Entity, _>(|entity| entity.to_bits())
        .enumerate()
    {
        let output = pick_output(placement, outputs.iter());
        let (position, units_per_logical_pixel) = output.map_or((Vec2::ZERO, 1.), |output| {
            (
                desktop_position(
                    output,
                    resolved_position(placement, Some(output)).unwrap_or_default(),
                ),
                output.units_per_logical_pixel,
            )
        });
        let next = Transform::from_xyz(position.x, -position.y, index as f32 * 1000.).with_scale(
            Vec3::splat(160. * placement.size_percent / 100. * units_per_logical_pixel),
        );
        transform.set_if_neq(next);
    }
}
