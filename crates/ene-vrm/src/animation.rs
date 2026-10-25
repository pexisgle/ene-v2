use crate::{
    VrmSystems,
    expressions::ExpressionName,
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
    platform::collections::{HashMap, HashSet},
    prelude::*,
};
use std::sync::Arc;

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
        for binding in &binds.bindings {
            let entity = binding.target;
            if let Ok(mut transform) = transforms.get_mut(entity)
                && transform.translation != Vec3::ZERO
            {
                transform.translation = Vec3::ZERO;
            }
        }
    }
}

fn apply(mut poses: Query<(&Pose, &RestPose, &mut Transform)>) {
    for (pose, rest, mut transform) in &mut poses {
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

/// Collects the animation target id for every node reachable from a scene.
///
/// Mirrors `bevy_gltf` 0.19.1's path derivation (`node_name` + `collect_path`)
/// so that curves from unselected scenes are grouped exactly as they are in the
/// native `AnimationClip`.
// ponytail: bevy_gltf keeps `node_name`/`collect_path` private; drop this copy
// when Bevy exposes node target paths. Recheck the path rules on upgrades.
fn collect_target(
    node: &gltf::Node,
    current: &[Name],
    targets: &mut HashMap<usize, AnimationTargetId>,
    visited: &mut HashSet<usize>,
) {
    let mut path = current.to_vec();
    path.push(Name::new(
        node.name()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("GltfNode{}", node.index())),
    ));
    visited.insert(node.index());
    for child in node.children() {
        if !visited.contains(&child.index()) {
            collect_target(&child, &path, targets, visited);
        }
    }
    targets.insert(node.index(), AnimationTargetId::from_names(path.iter()));
}

fn motion_targets(document: &gltf::Document) -> HashMap<usize, AnimationTargetId> {
    let mut targets = HashMap::default();
    for scene in document.scenes() {
        for node in scene.nodes() {
            collect_target(&node, &[], &mut targets, &mut HashSet::default());
        }
    }
    targets
}

fn motion_bones(motion: &schema::Motion) -> HashMap<usize, schema::HumanBoneName> {
    motion
        .humanoid
        .iter()
        .flat_map(|humanoid| humanoid.human_bones.0.iter())
        .filter_map(|(name, bone)| {
            let bone = bone.as_ref()?;
            Some((schema::index(bone.node?), *name))
        })
        .collect()
}

fn motion_expressions(motion: &schema::Motion) -> HashMap<usize, ExpressionName> {
    let mut expressions = HashMap::default();
    let Some(definitions) = &motion.expressions else {
        return expressions;
    };
    if let Some(preset) = &definitions.preset {
        for (name, expression) in &preset.0 {
            if let Some(node) = expression.node {
                expressions
                    .entry(schema::index(node))
                    .or_insert(ExpressionName::Preset(*name));
            }
        }
    }
    if let Some(custom) = &definitions.custom {
        for (name, expression) in custom {
            if let Some(node) = expression.node {
                expressions
                    .entry(schema::index(node))
                    .or_insert(ExpressionName::Custom(name.clone()));
            }
        }
    }
    expressions
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
    let hips = motion
        .humanoid
        .iter()
        .flat_map(|humanoid| humanoid.human_bones.0.get(&schema::HumanBoneName::Hips))
        .next()
        .and_then(|bone| bone.as_ref())
        .and_then(|bone| bone.node)
        .map(schema::index);
    let height = crate::runtime::hips_height(world, nodes, root_inv, hips);
    let bones = motion_bones(motion);
    let expressions = motion_expressions(motion);
    let (rot_prop, trans_prop) = (
        animated_field!(Transform::rotation),
        animated_field!(Transform::translation),
    );
    let (rot_id, trans_id) = (rot_prop.evaluator_id(), trans_prop.evaluator_id());
    let gaze_node = motion
        .look_at
        .as_ref()
        .and_then(|look_at| look_at.node)
        .map(schema::index);
    let targets = motion_targets(source);
    let mut clips = Vec::new();
    for animation in source.animations() {
        let label = format!("Animation{}", animation.index());
        let native = context
            .get_labeled(&label)
            .and_then(|asset| asset.get::<AnimationClip>())
            .ok_or_else(|| invalid("VRMA clip unavailable"))?;
        let mut clip = AnimationClip::default();
        clip.set_duration(native.duration());
        // Bevy groups native curves by `AnimationTargetId`, which it derives
        // from the node name path. Sibling nodes, and nodes in any other scene,
        // share that group when their paths match, so the source node can no
        // longer be recovered from the curve list. Only a curve we actually
        // adopt can be misattributed; collisions between unbound curves are
        // harmless.
        for channel in animation.channels() {
            let node = channel.target().node().index();
            let property = channel.target().property();
            let bone = bones.get(&node).copied();
            let expression = expressions.get(&node);
            let relevant = match property {
                gltf::animation::Property::Rotation => bone.is_some() || gaze_node == Some(node),
                gltf::animation::Property::Translation => {
                    bone == Some(schema::HumanBoneName::Hips) || expression.is_some()
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
            let native_id = targets
                .get(&node)
                .copied()
                .ok_or_else(|| invalid("VRMA animated node has no native target path"))?;
            if *id != native_id {
                return Err(invalid(format!(
                    "VRMA node {node} has multiple animation target paths"
                )));
            }
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
            // ponytail: reject instead of matching curves by channel order;
            // revisit if Bevy exposes per-node curve identity. The scan is
            // quadratic in channel count, which keeps VRMA-sized inputs simple.
            if let Some(other) = animation.channels().find(|other| {
                let target = other.target();
                target.node().index() != node
                    && target.property() == property
                    && targets.get(&target.node().index()) == Some(&native_id)
            }) {
                let other = other.target().node().index();
                return Err(invalid(format!(
                    "VRMA nodes {node} and {other} share the same animation target path"
                )));
            }
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
                let curve = Arc::new(curve.clone());
                clip.add_curve_to_target(
                    bone_target(bone),
                    AnimatableCurve::new(
                        AnimatedField::new_unchecked("rotation", |pose: &mut Pose| {
                            pose.rotation.get_or_insert(Quat::IDENTITY)
                        }),
                        FunctionCurve::new(domain, move |t| {
                            normalize_rotation(
                                local.rotation,
                                global_rot,
                                *curve
                                    .0
                                    .sample_clamped(t)
                                    .downcast::<Quat>()
                                    .expect("rotation curve"),
                            )
                        }),
                    ),
                );
            } else if eval == trans_id && bone == Some(schema::HumanBoneName::Hips) {
                let curve = Arc::new(curve.clone());
                let parent = global.affine() * local.compute_affine().inverse();
                clip.add_curve_to_target(
                    bone_target(schema::HumanBoneName::Hips),
                    AnimatableCurve::new(
                        AnimatedField::new_unchecked("translation", |pose: &mut Pose| {
                            pose.translation.get_or_insert(Vec3::ZERO)
                        }),
                        FunctionCurve::new(domain, move |t| {
                            parent.transform_vector3(
                                *curve
                                    .0
                                    .sample_clamped(t)
                                    .downcast::<Vec3>()
                                    .expect("translation curve")
                                    - local.translation,
                            ) / height
                        }),
                    ),
                );
            } else if eval == trans_id
                && let Some(expression) = expression
            {
                clip.add_variable_curve_to_target(expression_target(expression), curve.clone());
            } else if eval == rot_id && gaze_node == Some(node) {
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
