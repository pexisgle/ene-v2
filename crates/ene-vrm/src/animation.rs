use crate::{
    VrmSystems,
    loader::{invalid, require_node},
    schema,
};
use bevy::{
    animation::{
        AnimationTargetId, animated_field,
        animation_curves::{AnimatableCurve, AnimatableProperty, AnimatedField},
    },
    app::AnimationSystems,
    math::curve::FunctionCurve,
    platform::collections::HashMap,
    prelude::*,
};
use std::sync::Arc;

pub(crate) fn bone_target(name: &str) -> AnimationTargetId {
    AnimationTargetId::from_name(&Name::new(format!("VRMC_vrm/bone/{name}")))
}
pub(crate) fn expression_target(name: &str) -> AnimationTargetId {
    AnimationTargetId::from_name(&Name::new(format!("VRMC_vrm/expression/{name}")))
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

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct RestPose {
    pub local: Transform,
    pub global_rotation: Quat,
    pub translation_from_normalized: bevy::math::Mat3A,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Pose>()
        .register_type::<RestPose>()
        .add_systems(PostUpdate, reset.before(AnimationSystems))
        .add_systems(PostUpdate, apply.in_set(VrmSystems::Pose));
}

fn reset(
    mut poses: Query<&mut Pose>,
    roots: Query<&crate::expressions::Binds>,
    mut transforms: Query<&mut Transform, Changed<Transform>>,
) {
    for mut pose in &mut poses {
        if pose.rotation.is_some() || pose.translation.is_some() {
            *pose = Pose::default();
        }
    }
    for binds in &roots {
        for &entity in &binds.targets {
            if let Ok(mut transform) = transforms.get_mut(entity)
                && transform.translation != Vec3::ZERO
            {
                transform.translation = Vec3::ZERO;
            }
        }
    }
}

fn apply(poses: Query<(&Pose, &ChildOf)>, mut transforms: Query<(&RestPose, &mut Transform)>) {
    for (pose, target) in &poses {
        let Ok((rest, mut transform)) = transforms.get_mut(target.parent()) else {
            continue;
        };
        if let Some(rotation) = pose.rotation {
            transform.rotation =
                denormalize_rotation(rest.local.rotation, rest.global_rotation, rotation);
        }
        if let Some(translation) = pose.translation {
            transform.translation =
                rest.local.translation + rest.translation_from_normalized * translation;
        }
    }
}

fn normalize_rotation(local: Quat, global: Quat, value: Quat) -> Quat {
    global * local.inverse() * value * global.inverse()
}
pub(crate) fn denormalize_rotation(local: Quat, global: Quat, value: Quat) -> Quat {
    local * global.inverse() * value * global
}

pub(crate) fn retarget_motion(
    motion: &schema::Motion,
    source: &gltf::Document,
    nodes: &HashMap<usize, Entity>,
    context: &bevy::asset::LoadContext<'_>,
    world: &World,
    root: Entity,
) -> std::io::Result<Vec<(String, AnimationClip)>> {
    let root_inv = world
        .get::<GlobalTransform>(root)
        .expect("native scene transform")
        .affine()
        .inverse();
    let bones = motion.humanoid.as_ref().map(|h| &h.human_bones);
    let height = motion
        .humanoid
        .as_ref()
        .map_or(1., |h| h.hips_height(world, nodes, root_inv));
    let (rot_prop, trans_prop) = (
        animated_field!(Transform::rotation),
        animated_field!(Transform::translation),
    );
    let (rot_id, trans_id) = (rot_prop.evaluator_id(), trans_prop.evaluator_id());
    let gaze_node = motion.look_at.as_ref().map(|l| l.node);
    let mut clips = Vec::new();
    for animation in source.animations() {
        let label = format!("Animation{}", animation.index());
        let native = context
            .get_labeled(&label)
            .and_then(|x| x.get::<AnimationClip>())
            .ok_or_else(|| invalid("VRMA clip unavailable"))?;
        let mut clip = AnimationClip::default();
        clip.set_duration(native.duration());
        for channel in animation.channels() {
            let node = channel.target().node().index();
            let property = channel.target().property();
            let bone =
                bones.and_then(|b| b.iter().find_map(|(k, v)| (v.node == node).then_some(k)));
            let expr = motion
                .expressions
                .iter()
                .find_map(|(k, v)| (v.node == node).then_some(k));
            let gaze = gaze_node == Some(node);
            let relevant = match property {
                gltf::animation::Property::Rotation => bone.is_some() || gaze,
                gltf::animation::Property::Translation => {
                    bone.is_some_and(|b| b == "hips") || expr.is_some()
                }
                _ => false,
            };
            if !relevant {
                continue;
            }
            let entity = require_node(nodes, node)?;
            let id = world
                .get::<AnimationTargetId>(entity)
                .ok_or_else(|| invalid("VRMA animated node has no native target"))?;
            // Bevy skips channels it cannot decode (for example sparse sampler inputs).
            // Match by property/evaluator instead of channel position, and skip a channel
            // whose native curve is absent rather than failing the whole VRMA.
            let Some(curve) = native.curves().get(id).and_then(|curves| {
                curves.iter().find(|curve| match property {
                    gltf::animation::Property::Rotation => curve.0.evaluator_id() == rot_id,
                    gltf::animation::Property::Translation => curve.0.evaluator_id() == trans_id,
                    _ => false,
                })
            }) else {
                continue;
            };
            let domain = curve.0.domain();
            let (&local, global) = world
                .entity(entity)
                .components::<(&Transform, &GlobalTransform)>();
            let global = GlobalTransform::from(root_inv * global.affine());
            let global_rot = global.rotation().normalize();
            let eval = curve.0.evaluator_id();
            if eval == rot_id
                && let Some(bone) = bone
            {
                let c = Arc::new(curve.clone());
                clip.add_curve_to_target(
                    bone_target(bone),
                    AnimatableCurve::new(
                        AnimatedField::new_unchecked("rotation", |p: &mut Pose| {
                            p.rotation.get_or_insert(Quat::IDENTITY)
                        }),
                        FunctionCurve::new(domain, move |t| {
                            normalize_rotation(
                                local.rotation,
                                global_rot,
                                *c.0.sample_clamped(t)
                                    .downcast::<Quat>()
                                    .expect("rotation curve"),
                            )
                        }),
                    ),
                );
            } else if eval == trans_id && bone.is_some_and(|b| b == "hips") {
                let c = Arc::new(curve.clone());
                let parent = global.affine() * local.compute_affine().inverse();
                clip.add_curve_to_target(
                    bone_target("hips"),
                    AnimatableCurve::new(
                        AnimatedField::new_unchecked("translation", |p: &mut Pose| {
                            p.translation.get_or_insert(Vec3::ZERO)
                        }),
                        FunctionCurve::new(domain, move |t| {
                            parent.transform_vector3(
                                *c.0.sample_clamped(t)
                                    .downcast::<Vec3>()
                                    .expect("translation curve")
                                    - local.translation,
                            ) / height
                        }),
                    ),
                );
            } else if eval == trans_id
                && let Some(expr) = expr
            {
                clip.add_variable_curve_to_target(expression_target(expr), curve.clone());
            } else if eval == rot_id && motion.look_at.as_ref().map(|l| l.node) == Some(node) {
                clip.add_variable_curve_to_target(gaze_target(), curve.clone());
            }
        }
        if clip.curves().is_empty() {
            return Err(invalid("VRMA contains no supported animation channels"));
        }
        clips.push((label, clip));
    }
    if clips.is_empty() {
        return Err(invalid("VRMA requires an animation"));
    }
    Ok(clips)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_roundtrip_preserves_rest_axes() {
        let local = Quat::from_rotation_x(0.7);
        let global = Quat::from_rotation_z(-1.2) * local;
        let value = local * Quat::from_rotation_y(0.4);
        assert!(
            denormalize_rotation(local, global, normalize_rotation(local, global, value))
                .abs_diff_eq(value, 1e-5)
        );
    }
}
