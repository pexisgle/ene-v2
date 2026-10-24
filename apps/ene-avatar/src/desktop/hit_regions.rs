//! Hit regions derived from rendered mesh bounds.

use super::output::Output;
use crate::character::{AvatarState, CharacterId};
use bevy::{camera::primitives::Aabb, prelude::*};

#[derive(Component, Default, PartialEq)]
pub struct HitRegions(pub Vec<(Entity, Rect)>);

pub(super) fn regions(
    mut outputs: Query<(&Output, &mut Camera, &mut HitRegions)>,
    avatars: Query<(Entity, &AvatarState), With<CharacterId>>,
    parents: Query<&ChildOf>,
    meshes: Query<(Entity, &Aabb, &GlobalTransform, &InheritedVisibility), With<Mesh3d>>,
) {
    let geometry: Vec<_> = meshes
        .iter()
        .filter_map(|(entity, aabb, transform, visible)| {
            let (owner, state) = avatars
                .iter_many(std::iter::once(entity).chain(parents.iter_ancestors(entity)))
                .next()?;
            if *state != AvatarState::Ready || !visible.get() {
                return None;
            }
            // shortcut: mesh bounds include gaps and omit morph deformation; refine if input misses are observed.
            let matrix = transform.to_matrix();
            let center = matrix.transform_point3(aabb.center.into());
            let half = Mat3::from_mat4(matrix).abs() * Vec3::from(aabb.half_extents);
            Some((owner, Vec2::new(center.x, -center.y), half.truncate()))
        })
        .collect();
    for (output, mut camera, mut regions) in &mut outputs {
        let had = !regions.0.is_empty();
        let mut next = Vec::new();
        for &(actor, center, half) in &geometry {
            let center = (center - output.origin) / output.units_per_logical_pixel;
            let half = half / output.units_per_logical_pixel + Vec2::splat(2.);
            let limit = output.size / output.units_per_logical_pixel;
            let min = (center - half).max(Vec2::ZERO);
            let max = (center + half).min(limit);
            if min.is_finite() && max.is_finite() && min.cmplt(max).all() {
                next.push((actor, Rect::from_corners(min, max)));
            }
        }
        // Clear the previous image once before disabling an empty camera.
        let active = had || !next.is_empty();
        regions.set_if_neq(HitRegions(next));
        if camera.is_active != active {
            camera.is_active = active;
        }
    }
}
