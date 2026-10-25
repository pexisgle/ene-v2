use crate::{
    VrmSystems,
    animation::{self, RestPose},
    expressions::{self, ExpressionName},
    schema,
};
use bevy::{
    ecs::system::SystemChangeTick, math::Affine3A, prelude::*, transform::helper::TransformHelper,
};

/// Optional world-space target. `None` allows VRMA gaze animation.
#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub struct LookAt {
    pub target: Option<Vec3>,
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone, opaque)]
pub(crate) struct Gaze {
    pub(crate) definition: schema::LookAt,
    #[entities]
    pub(crate) input: Entity,
    pub(crate) active: bool,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<LookAt>()
        .register_type::<Gaze>()
        .add_systems(PostUpdate, reset_gaze.before(bevy::app::AnimationSystems))
        .add_systems(PostUpdate, look_at.in_set(VrmSystems::LookAt));
}

fn reset_gaze(
    mut roots: Query<(&super::Vrm, &mut Gaze)>,
    mut transforms: Query<(&RestPose, &mut Transform)>,
) {
    for (vrm, mut gaze) in &mut roots {
        gaze.set_changed();
        if !gaze.active {
            continue;
        }
        for name in [
            schema::HumanBoneName::LeftEye,
            schema::HumanBoneName::RightEye,
        ] {
            if let Some(eye) = vrm.bone(name)
                && let Ok((rest, mut t)) = transforms.get_mut(eye)
            {
                t.rotation = rest.local.rotation;
            }
        }
        gaze.active = false;
    }
}

/// Resolves a VRM range map with the three-vrm defaults.
pub(crate) fn remap(
    map: Option<&schema::LookAtRangeMap>,
    kind: schema::LookAtType,
    angle: f32,
) -> f32 {
    let default_scale = if matches!(kind, schema::LookAtType::Bone) {
        10.
    } else {
        1.
    };
    let (input_max, output_scale) = map.map_or((90., default_scale), |map| {
        (
            map.input_max_value.unwrap_or(90.),
            map.output_scale.unwrap_or(default_scale),
        )
    });
    (angle.abs() / (input_max as f32).max(f32::EPSILON)).min(1.) * output_scale as f32
}

pub(crate) fn head_offset(definition: &schema::LookAt) -> Vec3 {
    definition
        .offset_from_head_bone
        .as_deref()
        .and_then(|offset| <[f64; 3]>::try_from(offset).ok())
        .map_or(Vec3::new(0., 0.06, 0.), |[x, y, z]| {
            Vec3::new(x as f32, y as f32, z as f32)
        })
}

/// Returns yaw and pitch in the rest-pose head frame.
///
/// The LookAt target lives in world space, while `offsetFromHeadBone` and the
/// rest pose are defined under the head. Invert the head's full affine
/// transform, not just its rotation, so a non-uniform scale does not distort
/// the direction.
fn gaze_angles(head: Affine3A, rest_rotation: Quat, target: Vec3, offset: Vec3) -> Vec2 {
    let dir = rest_rotation * (head.inverse().transform_point3(target) - offset);
    Vec2::new(
        dir.x.atan2(dir.z).to_degrees(),
        dir.y.atan2(dir.xz().length()).to_degrees(),
    )
}

#[allow(clippy::type_complexity)]
fn look_at(
    mut roots: Query<(
        &super::Vrm,
        &LookAt,
        &mut Gaze,
        Option<&crate::ExpressionWeights>,
        Option<&crate::expressions::Binds>,
    )>,
    rests: Query<&RestPose>,
    mut transforms: ParamSet<(
        TransformHelper,
        Query<&mut Transform>,
        Query<Ref<Transform>>,
    )>,
    ticks: SystemChangeTick,
) {
    for (vrm, look, mut gaze, manual, binds) in &mut roots {
        let Some(head) = vrm.bone(schema::HumanBoneName::Head) else {
            continue;
        };
        let angles = if let Some(target) = look.target {
            let (Ok(rest), Ok(head)) = (
                rests.get(head),
                transforms.p0().compute_global_transform(head),
            ) else {
                continue;
            };
            gaze_angles(
                head.affine(),
                rest.global_rotation,
                target,
                head_offset(&gaze.definition),
            )
        } else if let Ok(input) = transforms.p2().get(gaze.input)
            && input
                .last_changed()
                .is_newer_than(gaze.last_changed(), ticks.this_run())
        {
            let (yaw, pitch, _) = input.rotation.to_euler(EulerRot::YXZ);
            Vec2::new(yaw.to_degrees(), -pitch.to_degrees())
        } else {
            continue;
        };
        let kind = gaze
            .definition
            .look_at_type
            .unwrap_or(schema::LookAtType::Bone);
        let bone = matches!(kind, schema::LookAtType::Bone);
        gaze.active = bone;
        let definition = &gaze.definition;
        let pitch = if angles.y > 0. {
            remap(definition.range_map_vertical_up.as_ref(), kind, angles.y)
        } else {
            -remap(definition.range_map_vertical_down.as_ref(), kind, angles.y)
        };
        if bone {
            let p2 = transforms.p2();
            let scale = binds.map_or(1., |binds| {
                let weights = binds.weight(manual, |entity| {
                    p2.get(entity).ok().map(|transform| transform.translation.x)
                });
                expressions::look_at_scale(binds, &weights)
            });
            let pitch = pitch * scale;
            for (name, inner) in [
                (schema::HumanBoneName::LeftEye, angles.x < 0.),
                (schema::HumanBoneName::RightEye, angles.x > 0.),
            ] {
                let map = if inner {
                    definition.range_map_horizontal_inner.as_ref()
                } else {
                    definition.range_map_horizontal_outer.as_ref()
                };
                let yaw = remap(map, kind, angles.x) * angles.x.signum() * scale;
                if let Some(eye) = vrm.bone(name)
                    && let Ok(rest) = rests.get(eye)
                    && let Ok(mut t) = transforms.p1().get_mut(eye)
                {
                    t.rotation = animation::denormalize_rotation(
                        rest.local.rotation,
                        rest.global_rotation,
                        Quat::from_euler(EulerRot::YXZ, yaw.to_radians(), -pitch.to_radians(), 0.),
                    );
                }
            }
        } else {
            let (h, v) = (
                remap(
                    definition.range_map_horizontal_outer.as_ref(),
                    kind,
                    angles.x,
                ),
                pitch.abs(),
            );
            for (name, w) in [
                (
                    schema::ExpressionPresetName::LookLeft,
                    if angles.x > 0. { h } else { 0. },
                ),
                (
                    schema::ExpressionPresetName::LookRight,
                    if angles.x < 0. { h } else { 0. },
                ),
                (
                    schema::ExpressionPresetName::LookUp,
                    if angles.y > 0. { v } else { 0. },
                ),
                (
                    schema::ExpressionPresetName::LookDown,
                    if angles.y < 0. { v } else { 0. },
                ),
            ] {
                if let Some(channel) =
                    binds.and_then(|binds| binds.target(&ExpressionName::Preset(name)))
                    && let Ok(mut transform) = transforms.p1().get_mut(channel)
                {
                    transform.translation.x = w;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_uniform_head_scale_is_inverted() {
        let head = Affine3A::from_scale_rotation_translation(
            Vec3::new(2., 1., 1.),
            Quat::IDENTITY,
            Vec3::ZERO,
        );
        let angles = gaze_angles(head, Quat::IDENTITY, Vec3::new(2., 0., 1.), Vec3::ZERO);
        assert!((angles.x - 45.).abs() < 1e-4, "{angles:?}");
        assert!(angles.y.abs() < 1e-4, "{angles:?}");
    }
}
