use crate::{
    VrmSystems,
    animation::{self, RestPose},
    expressions, schema,
};
use bevy::{
    ecs::{entity::MapEntities, system::SystemChangeTick},
    prelude::*,
    transform::helper::TransformHelper,
};

/// Optional world-space target. `None` allows VRMA gaze animation.
#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub struct LookAt {
    pub target: Option<Vec3>,
}

#[derive(Component, Clone, Reflect, MapEntities)]
#[reflect(Component, Clone)]
pub(crate) struct Gaze {
    pub(crate) definition: schema::Gaze,
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
        for name in ["leftEye", "rightEye"] {
            if let Some(eye) = vrm.bone(name)
                && let Ok((rest, mut t)) = transforms.get_mut(eye)
            {
                t.rotation = rest.local.rotation;
            }
        }
        gaze.active = false;
    }
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
        let Some(head) = vrm.bone("head") else {
            continue;
        };
        let angles = if let Some(target) = look.target {
            let (Ok(rest), Ok(head)) = (
                rests.get(head),
                transforms.p0().compute_global_transform(head),
            ) else {
                continue;
            };
            let dir = (head.rotation().normalize() * rest.global_rotation.inverse()).inverse()
                * (target - head.transform_point(gaze.definition.offset_from_head_bone));
            Vec2::new(
                dir.x.atan2(dir.z).to_degrees(),
                dir.y.atan2(dir.xz().length()).to_degrees(),
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
        gaze.active = matches!(gaze.definition.r#type, schema::GazeType::Bone);
        let def = &gaze.definition;
        let pitch = if angles.y > 0. {
            def.vertical_up().map(angles.y)
        } else {
            -def.vertical_down().map(angles.y)
        };
        match def.r#type {
            schema::GazeType::Bone => {
                let p2 = transforms.p2();
                let scale = binds.map_or(1., |b| {
                    expressions::look_at_scale(
                        &b.definitions,
                        b.weight(manual, |e| p2.get(e).ok().map(|t| t.translation.x)),
                    )
                });
                let pitch = pitch * scale;
                for (name, inner) in [("leftEye", angles.x < 0.), ("rightEye", angles.x > 0.)] {
                    let map = if inner {
                        def.horizontal_inner()
                    } else {
                        def.horizontal_outer()
                    };
                    let yaw = map.map(angles.x) * angles.x.signum() * scale;
                    if let Some(eye) = vrm.bone(name)
                        && let Ok(rest) = rests.get(eye)
                        && let Ok(mut t) = transforms.p1().get_mut(eye)
                    {
                        t.rotation = animation::denormalize_rotation(
                            rest.local.rotation,
                            rest.global_rotation,
                            Quat::from_euler(
                                EulerRot::YXZ,
                                yaw.to_radians(),
                                -pitch.to_radians(),
                                0.,
                            ),
                        );
                    }
                }
            }
            schema::GazeType::Expression => {
                let (h, v) = (def.horizontal_outer().map(angles.x), pitch.abs());
                for (name, w) in [
                    ("lookLeft", if angles.x > 0. { h } else { 0. }),
                    ("lookRight", if angles.x < 0. { h } else { 0. }),
                    ("lookUp", if angles.y > 0. { v } else { 0. }),
                    ("lookDown", if angles.y < 0. { v } else { 0. }),
                ] {
                    if let Some(ch) = binds.and_then(|b| b.target(name))
                        && let Ok(mut t) = transforms.p1().get_mut(ch)
                    {
                        t.translation.x = w;
                    }
                }
            }
        }
    }
}
