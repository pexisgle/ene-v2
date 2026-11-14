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
    curve::FunctionCurve,
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

/// Collects the animation target id for every node reachable from a scene.
///
/// Mirrors `bevy_gltf` 0.20.0's path derivation (`node_name` + `collect_path`)
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
            let frame = RestFrame {
                local: local.rotation,
                global: global.rotation().normalize(),
            };
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
                            frame.normalize(
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
