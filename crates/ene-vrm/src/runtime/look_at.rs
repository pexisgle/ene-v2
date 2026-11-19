use crate::{
    VrmSystems,
    animation::{ExpressionInput, GazeInput, RestPose},
    expressions::{self, ExpressionName},
    schema,
};
use bevy::prelude::*;

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone, opaque)]
pub(crate) struct Gaze {
    pub(crate) definition: schema::LookAt,
    #[entities]
    pub(crate) input: Entity,
}

/// Direct gaze angles in degrees (`yaw_deg`: +left/-right, `pitch_deg`: +up/-down).
/// When present on a VRM root entity, overrides [`GazeInput`] from animation clips.
#[derive(Component, Clone, Copy, Debug, PartialEq, Reflect)]
#[reflect(Component, Clone)]
pub struct GazeOverride {
    pub yaw_deg: f32,
    pub pitch_deg: f32,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Gaze>()
        .register_type::<GazeOverride>()
        .add_systems(PostUpdate, look_at.in_set(VrmSystems::LookAt));
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
    roots: Query<(
        &super::Vrm,
        &Gaze,
        Option<&GazeOverride>,
        Option<&expressions::Binds>,
    )>,
    rests: Query<&RestPose>,
    gazes: Query<&GazeInput>,
    mut inputs: Query<&mut ExpressionInput>,
    mut transforms: Query<&mut Transform>,
) {
    for (vrm, gaze, gaze_override, binds) in &roots {
        let angles = if let Some(over) = gaze_override
            && over.yaw_deg.is_finite()
            && over.pitch_deg.is_finite()
        {
            Vec2::new(
                over.yaw_deg.clamp(-90., 90.),
                over.pitch_deg.clamp(-60., 60.),
            )
        } else {
            let Some(rotation) = gazes.get(gaze.input).ok().and_then(|input| input.rotation) else {
                continue;
            };
            let (yaw, pitch, _) = rotation.to_euler(EulerRot::YXZ);
            Vec2::new(yaw.to_degrees(), -pitch.to_degrees())
        };
        let kind = gaze
            .definition
            .look_at_type
            .unwrap_or(schema::LookAtType::Bone);
        let bone = matches!(kind, schema::LookAtType::Bone);
        let definition = &gaze.definition;
        let pitch = if angles.y > 0. {
            remap(definition.range_map_vertical_up.as_ref(), kind, angles.y)
        } else {
            -remap(definition.range_map_vertical_down.as_ref(), kind, angles.y)
        };
        if bone {
            let scale = binds.map_or(1., |binds| {
                let weights = expressions::weight(|entity| {
                    inputs.get(entity).ok().and_then(|input| input.value)
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
                    && let Ok(mut t) = transforms.get_mut(eye)
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
                    && let Ok(mut input) = inputs.get_mut(channel)
                {
                    input.value = Some(w);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::animation::{self, Pose, RestFrame};
    use bevy::platform::collections::HashMap;

    #[derive(Resource)]
    struct Sample(Option<Quat>);

    #[test]
    fn neutral_gaze_overrides_eye_pose_but_missing_gaze_does_not() {
        let mut app = App::new();
        animation::setup(&mut app);
        setup(&mut app);
        app.configure_sets(PostUpdate, VrmSystems::Pose.before(VrmSystems::LookAt));
        app.insert_resource(Sample(None));
        app.add_systems(
            PostUpdate,
            (|sample: Res<Sample>,
              mut poses: Query<&mut Pose>,
              mut gazes: Query<&mut GazeInput>| {
                for mut pose in &mut poses {
                    pose.rotation = Some(Quat::from_rotation_x(0.5));
                }
                for mut gaze in &mut gazes {
                    gaze.rotation = sample.0;
                }
            })
            .after(bevy::app::AnimationSystems)
            .before(VrmSystems::Pose),
        );
        let eye = app
            .world_mut()
            .spawn((
                Transform::default(),
                Pose::default(),
                RestPose {
                    local: Transform::default(),
                    frame: RestFrame {
                        local: Quat::IDENTITY,
                        global: Quat::IDENTITY,
                    },
                    translation_from_normalized: bevy::math::Mat3A::IDENTITY,
                },
            ))
            .id();
        let input = app.world_mut().spawn(GazeInput::default()).id();
        app.world_mut().spawn((super::super::Vrm {
            bones: HashMap::from([(schema::HumanBoneName::LeftEye, eye)]),
            meta: serde_json::from_value(serde_json::json!({"name":"test","authors":["test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"})).unwrap(),
        }, Gaze { definition: serde_json::from_value(serde_json::json!({"type":"bone"})).unwrap(), input }));
        app.update();
        assert!(
            app.world()
                .get::<Transform>(eye)
                .unwrap()
                .rotation
                .abs_diff_eq(Quat::from_rotation_x(0.5), 1e-6)
        );
        app.world_mut().resource_mut::<Sample>().0 = Some(Quat::IDENTITY);
        app.update();
        assert!(
            app.world()
                .get::<Transform>(eye)
                .unwrap()
                .rotation
                .abs_diff_eq(Quat::IDENTITY, 1e-6)
        );
        app.world_mut().resource_mut::<Sample>().0 = None;
        app.update();
        assert!(
            app.world()
                .get::<Transform>(eye)
                .unwrap()
                .rotation
                .abs_diff_eq(Quat::from_rotation_x(0.5), 1e-6)
        );
    }

    #[test]
    fn f14_gaze_override_rotates_eye_bones_within_range_map() {
        let mut app = App::new();
        animation::setup(&mut app);
        setup(&mut app);
        app.configure_sets(PostUpdate, VrmSystems::Pose.before(VrmSystems::LookAt));
        let eye = app
            .world_mut()
            .spawn((
                Transform::default(),
                Pose::default(),
                RestPose {
                    local: Transform::default(),
                    frame: RestFrame {
                        local: Quat::IDENTITY,
                        global: Quat::IDENTITY,
                    },
                    translation_from_normalized: bevy::math::Mat3A::IDENTITY,
                },
            ))
            .id();
        let input = app.world_mut().spawn(GazeInput::default()).id();
        let root = app
            .world_mut()
            .spawn((
                super::super::Vrm {
                    bones: HashMap::from([(schema::HumanBoneName::LeftEye, eye)]),
                    meta: serde_json::from_value(serde_json::json!({
                        "name": "test",
                        "authors": ["test"],
                        "licenseUrl": "https://vrm.dev/licenses/1.0/"
                    }))
                    .unwrap(),
                },
                Gaze {
                    definition: serde_json::from_value(serde_json::json!({"type": "bone"}))
                        .unwrap(),
                    input,
                },
                GazeOverride {
                    yaw_deg: 45.,
                    pitch_deg: 30.,
                },
            ))
            .id();
        app.update();
        let rotated = app.world().get::<Transform>(eye).unwrap().rotation;
        let (yaw, pitch, _) = rotated.to_euler(EulerRot::YXZ);
        // Default VRM bone range map scales 90 deg -> 10 deg: 45 deg -> 5 deg yaw, 30 deg -> ~3.33 deg pitch.
        assert!(
            (yaw.to_degrees() - 5.0).abs() < 0.05,
            "yaw={}",
            yaw.to_degrees()
        );
        assert!(
            ((-pitch).to_degrees() - (30.0 / 9.0)).abs() < 0.05,
            "pitch={}",
            (-pitch).to_degrees()
        );

        // Removing GazeOverride lets the eye rest at neutral when no clip gaze is present.
        app.world_mut().entity_mut(root).remove::<GazeOverride>();
        app.world_mut().get_mut::<Transform>(eye).unwrap().rotation = Quat::IDENTITY;
        app.update();
        assert!(
            app.world()
                .get::<Transform>(eye)
                .unwrap()
                .rotation
                .abs_diff_eq(Quat::IDENTITY, 1e-6)
        );
    }
}
