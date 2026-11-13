use crate::{
    VrmSystems,
    animation::RestPose,
    expressions::{self, ExpressionName},
    schema,
};
use bevy::{ecs::system::SystemChangeTick, prelude::*};

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone, opaque)]
pub(crate) struct Gaze {
    pub(crate) definition: schema::LookAt,
    #[entities]
    pub(crate) input: Entity,
    pub(crate) active: bool,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Gaze>()
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

#[allow(clippy::type_complexity)]
fn look_at(
    mut roots: Query<(&super::Vrm, &mut Gaze, Option<&expressions::Binds>)>,
    rests: Query<&RestPose>,
    mut transforms: ParamSet<(Query<&mut Transform>, Query<Ref<Transform>>)>,
    ticks: SystemChangeTick,
) {
    for (vrm, mut gaze, binds) in &mut roots {
        let angles = if let Ok(input) = transforms.p1().get(gaze.input)
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
            let input = transforms.p1();
            let scale = binds.map_or(1., |binds| {
                let weights = expressions::weight(|entity| {
                    input
                        .get(entity)
                        .ok()
                        .map(|transform| transform.translation.x)
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
                    && let Ok(mut t) = transforms.p0().get_mut(eye)
                {
                    t.rotation = rest.frame.localize(Quat::from_euler(
                        EulerRot::YXZ,
                        yaw.to_radians(),
                        -pitch.to_radians(),
                        0.,
                    ));
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
                    && let Ok(mut transform) = transforms.p0().get_mut(channel)
                {
                    transform.translation.x = w;
                }
            }
        }
    }
}
