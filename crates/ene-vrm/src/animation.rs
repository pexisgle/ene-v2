use crate::{VrmSystems, expressions::ExpressionName, schema};
use bevy::{animation::AnimationTargetId, app::AnimationSystems, prelude::*};

pub(crate) fn bone_target(name: schema::HumanBoneName) -> AnimationTargetId {
    // These ids are internal to this crate; attach and retargeting only need to
    // agree within one binary.
    AnimationTargetId::from_name(&Name::new(format!("VRMC_vrm/bone/{name:?}")))
}
pub(crate) fn expression_target(name: &ExpressionName) -> AnimationTargetId {
    AnimationTargetId::from_name(&Name::new(format!("VRMC_vrm/expression/{name:?}")))
}
pub(crate) fn gaze_target() -> AnimationTargetId {
    AnimationTargetId::from_name(&Name::new("VRMC_vrm/lookAt"))
}

#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct Pose {
    pub rotation: Option<Quat>,
    pub translation: Option<Vec3>,
}

#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct ExpressionInput {
    pub value: Option<f32>,
}

#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct GazeInput {
    pub rotation: Option<Quat>,
}

/// The bind-pose orientation of one bone, used to move rotations between the
/// pose space a VRMA clip was authored in and this model's local bone space.
///
/// `specification/VRMC_vrm_animation-1.0/how_to_transform_human_pose.md`
/// defines the intermediate `NormalizedLocalRotation` for a source model with
/// world rest rotation `W` and local rest rotation `L`:
///
/// ```text
/// model     = W * L^-1 * local * W^-1
/// local     = L * W^-1 * model * W
/// ```
///
/// `model` is the rotation a clip stores; `local` is the bone's local
/// rotation. Using a different `W`/`L` on each side lets one clip drive models
/// whose T-poses differ.
#[derive(Clone, Copy, Reflect)]
pub(crate) struct RestFrame {
    pub local: Quat,
    pub global: Quat,
}

impl RestFrame {
    /// Converts a clip rotation into this model's local bone space.
    pub(crate) fn localize(self, rotation: Quat) -> Quat {
        self.local * self.global.inverse() * rotation * self.global
    }

    /// Converts this model's local bone rotation into clip space.
    pub(crate) fn normalize(self, rotation: Quat) -> Quat {
        self.global * self.local.inverse() * rotation * self.global.inverse()
    }
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct RestPose {
    pub local: Transform,
    pub frame: RestFrame,
    pub translation_from_normalized: bevy::math::Mat3A,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Pose>()
        .register_type::<RestPose>()
        .register_type::<ExpressionInput>()
        .register_type::<GazeInput>()
        .add_systems(PostUpdate, reset.before(AnimationSystems))
        .add_systems(PostUpdate, apply.in_set(VrmSystems::Pose));
}

fn reset(
    mut poses: Query<&mut Pose>,
    mut expressions: Query<&mut ExpressionInput>,
    mut gazes: Query<&mut GazeInput>,
) {
    for mut pose in &mut poses {
        if pose.rotation.is_some() || pose.translation.is_some() {
            *pose = Pose::default();
        }
    }
    for mut input in &mut expressions {
        input.value = None;
    }
    for mut input in &mut gazes {
        input.rotation = None;
    }
}

fn apply(mut poses: Query<(&Pose, &RestPose, &mut Transform)>) {
    for (pose, rest, mut transform) in &mut poses {
        // A stopped animation leaves no input for its bones. Restore the rest
        // pose instead of freezing the last frame, so stopping a clip and
        // switching to one that does not bind a bone both return to rest.
        let rotation = pose.rotation.map_or(rest.local.rotation, |rotation| {
            rest.frame.localize(rotation)
        });
        let translation = pose
            .translation
            .map_or(rest.local.translation, |translation| {
                rest.local.translation + rest.translation_from_normalized * translation
            });
        transform.set_if_neq(Transform {
            rotation,
            translation,
            ..*transform
        });
    }
}

#[derive(Component)]
#[component(clone_behavior = Ignore)]
pub(crate) struct NativeNode(pub usize);

#[derive(Component)]
#[component(clone_behavior = Ignore)]
pub(crate) struct NativeSceneRoot;

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    fn app_with_bone(rest: Transform) -> (App, Entity) {
        let mut app = App::new();
        let entity = app
            .world_mut()
            .spawn((
                Transform::default(),
                RestPose {
                    local: rest,
                    frame: RestFrame {
                        local: rest.rotation,
                        global: Quat::IDENTITY,
                    },
                    translation_from_normalized: bevy::math::Mat3A::IDENTITY,
                },
                Pose::default(),
            ))
            .id();
        (app, entity)
    }

    #[test]
    fn bones_without_animation_input_return_to_rest() {
        let rest = Transform::from_xyz(0., 1., 0.).with_rotation(Quat::from_rotation_z(0.4));
        let (mut app, entity) = app_with_bone(rest);

        app.world_mut().get_mut::<Pose>(entity).unwrap().rotation =
            Some(Quat::from_rotation_x(0.5));
        app.world_mut()
            .run_system_once(apply)
            .expect("apply animation pose");
        let animated = *app.world().get::<Transform>(entity).unwrap();
        assert!(
            animated
                .rotation
                .abs_diff_eq(rest.rotation * Quat::from_rotation_x(0.5), 1e-6)
        );
        assert!(
            animated.translation.abs_diff_eq(rest.translation, 1e-6),
            "a rotation-only pose must not inherit a previous translation"
        );

        {
            let mut pose = app.world_mut().get_mut::<Pose>(entity).unwrap();
            pose.rotation = Some(Quat::from_rotation_x(0.5));
            pose.translation = Some(Vec3::new(2., 3., 4.));
        }
        app.world_mut()
            .run_system_once(apply)
            .expect("apply animation pose");
        let animated = *app.world().get::<Transform>(entity).unwrap();
        assert!(
            animated
                .translation
                .abs_diff_eq(rest.translation + Vec3::new(2., 3., 4.), 1e-6)
        );

        // The next frame has no animation input at all.
        *app.world_mut().get_mut::<Pose>(entity).unwrap() = Pose::default();
        app.world_mut()
            .run_system_once(apply)
            .expect("apply rest pose");
        let at_rest = *app.world().get::<Transform>(entity).unwrap();
        assert!(at_rest.rotation.abs_diff_eq(rest.rotation, 1e-6));
        assert!(at_rest.translation.abs_diff_eq(rest.translation, 1e-6));
        assert!(
            at_rest.scale.abs_diff_eq(Vec3::ONE, 1e-6),
            "scale is untouched"
        );
    }
}
