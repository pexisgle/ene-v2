//! `VRMC_node_constraint` (1.0) evaluation.
//!
//! A constrained node copies one aspect of a source node's local rotation:
//!
//! - [`Rule::Copy`] transfers the whole rotation delta,
//! - [`Rule::Twist`] keeps only the part of the delta that turns around one
//!   axis, and
//! - [`Rule::PointAt`] rotates an axis of the node towards another node.
//!
//! The spec evaluates every rule against rest transforms ("Local-Local" for
//! copy/roll, world positions for aim), so this module resolves the rest pairs
//! once while the scene is still in its bind pose and then evaluates the
//! animated local transforms each frame. Bindings that read a node another
//! binding writes are ordered after that writer, which is why the plan is a
//! stable topological order rather than the extension's document order.
//!
//! Reference: `specification/VRMC_node_constraint-1.0/README.md` in the VRM
//! specification.

use crate::{
    VrmSystems,
    loader::{invalid, require_node},
    schema,
};
use bevy::{
    ecs::entity::MapEntities, platform::collections::HashMap, prelude::*,
    transform::helper::TransformHelper,
};
use std::collections::VecDeque;

/// What a constrained node takes from its source.
#[derive(Clone, Copy, Reflect)]
pub(crate) enum Rule {
    /// `constraint.rotation`: copy the source's rest-to-current delta.
    Copy,
    /// `constraint.roll`: copy only the twist around the axis.
    Twist(Vec3),
    /// `constraint.aim`: turn the axis towards the source's world position.
    PointAt(Vec3),
}

/// One resolved constraint. Rest rotations are captured in the bind pose.
#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Binding {
    #[entities]
    pub(crate) target: Entity,
    #[entities]
    pub(crate) source: Entity,
    pub(crate) target_rest: Quat,
    pub(crate) source_rest: Quat,
    pub(crate) rule: Rule,
    pub(crate) weight: f32,
}

/// Constraints of one scene root, already in evaluation order.
#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct ConstraintRig(#[entities] pub(crate) Vec<Binding>);

pub(crate) fn setup(app: &mut App) {
    app.register_type::<ConstraintRig>()
        .add_systems(PostUpdate, run_constraints.in_set(VrmSystems::Constraints));
}

/// A definition waiting for its dependency order.
struct Pending {
    binding: Binding,
    /// Nodes this binding reads besides its source: for aim, the world
    /// position of both nodes depends on their whole ancestor chains.
    reads: Vec<Entity>,
}

pub(crate) fn compile_constraints(
    definitions: &[(usize, schema::NodeConstraint)],
    world: &mut World,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<ConstraintRig> {
    let mut parent_query = world.query::<&ChildOf>();
    let parents = parent_query.query(world);
    let mut pending = Vec::with_capacity(definitions.len());
    for (target, definition) in definitions {
        let (source, weight, rule) = match (
            &definition.constraint.rotation,
            &definition.constraint.roll,
            &definition.constraint.aim,
        ) {
            (Some(copy), None, None) => (copy.source, copy.weight.unwrap_or(1.), Rule::Copy),
            (None, Some(roll), None) => (
                roll.source,
                roll.weight.unwrap_or(1.),
                Rule::Twist(roll.roll_axis.vector()),
            ),
            (None, None, Some(aim)) => (
                aim.source,
                aim.weight.unwrap_or(1.),
                Rule::PointAt(aim.aim_axis.vector()),
            ),
            _ => {
                return Err(invalid(
                    "VRMC constraint requires exactly one of rotation, roll or aim",
                ));
            }
        };
        let (target, source) = (require_node(nodes, *target)?, require_node(nodes, source)?);
        if !weight.is_finite() || !(0. ..=1.).contains(&weight) {
            return Err(invalid("VRMC constraint weight out of range"));
        }
        let mut reads = vec![source];
        if matches!(rule, Rule::PointAt(_)) {
            for entity in [source, target] {
                reads.extend(parents.iter_ancestors(entity));
            }
        }
        pending.push(Pending {
            binding: Binding {
                target,
                source,
                target_rest: world.entity(target).components::<&Transform>().rotation,
                source_rest: world.entity(source).components::<&Transform>().rotation,
                rule,
                weight,
            },
            reads,
        });
    }
    let order = dependency_order(&pending)?;
    Ok(ConstraintRig(
        order
            .into_iter()
            .map(|index| pending[index].binding.clone())
            .collect(),
    ))
}

/// Orders bindings so a reader never runs before the binding that writes what
/// it reads. Kahn's algorithm also reports the cycles the spec forbids.
fn dependency_order(pending: &[Pending]) -> std::io::Result<Vec<usize>> {
    // A node carries at most one constraint, so one writer per target is
    // enough. A binding that reads its own output becomes a self edge and is
    // reported as a cycle, as the spec requires.
    let mut writer: HashMap<Entity, usize> = HashMap::default();
    for (index, item) in pending.iter().enumerate() {
        writer.insert(item.binding.target, index);
    }
    let mut followers = vec![Vec::new(); pending.len()];
    let mut prerequisites = vec![0usize; pending.len()];
    for (index, item) in pending.iter().enumerate() {
        let mut read_by: Vec<usize> = item
            .reads
            .iter()
            .filter_map(|entity| writer.get(entity).copied())
            .collect();
        read_by.sort_unstable();
        read_by.dedup();
        for prerequisite in read_by {
            // Every prerequisite writes a node this binding reads, including
            // the binding itself for a self reference.
            followers[prerequisite].push(index);
            prerequisites[index] += 1;
        }
    }
    let mut ready: VecDeque<usize> = (0..pending.len())
        .filter(|&index| prerequisites[index] == 0)
        .collect();
    let mut order = Vec::with_capacity(pending.len());
    while let Some(next) = ready.pop_front() {
        order.push(next);
        for &follower in &followers[next] {
            prerequisites[follower] -= 1;
            if prerequisites[follower] == 0 {
                ready.push_back(follower);
            }
        }
    }
    if order.len() == pending.len() {
        Ok(order)
    } else {
        Err(invalid("VRMC constraint cycle"))
    }
}

fn run_constraints(
    rigs: Query<&ConstraintRig>,
    hierarchy: Query<&ChildOf>,
    mut transforms: ParamSet<(TransformHelper, Query<&mut Transform>)>,
) {
    for rig in &rigs {
        for binding in &rig.0 {
            let source_rotation = transforms
                .p1()
                .get(binding.source)
                .map_or(binding.source_rest, |transform| transform.rotation);
            let rotation = match binding.rule {
                Rule::Copy => binding.target_rest * binding.source_rest.inverse() * source_rotation,
                Rule::Twist(axis) => twist_about(
                    axis,
                    binding.target_rest,
                    binding.source_rest,
                    source_rotation,
                ),
                Rule::PointAt(axis) => {
                    let (Ok(source), Ok(target)) = (
                        transforms.p0().compute_global_transform(binding.source),
                        transforms.p0().compute_global_transform(binding.target),
                    ) else {
                        continue;
                    };
                    // The aim axis is evaluated in the target's parent space,
                    // which is where the local rotation it writes lives.
                    let parent_rotation = hierarchy
                        .get(binding.target)
                        .ok()
                        .and_then(|parent| {
                            transforms
                                .p0()
                                .compute_global_transform(parent.parent())
                                .ok()
                        })
                        .map_or(Quat::IDENTITY, |parent| parent.rotation().normalize());
                    let Some(rotation) = point_at(
                        parent_rotation,
                        binding.target_rest,
                        axis,
                        source.translation(),
                        target.translation(),
                    ) else {
                        continue;
                    };
                    rotation
                }
            };
            if let Ok(mut target) = transforms.p1().get_mut(binding.target) {
                target.rotation = binding.target_rest.slerp(rotation, binding.weight);
            }
        }
    }
}

/// The rotation that keeps only the part of the source delta turning around
/// `axis`. The delta is taken from the source's rest orientation into the
/// target's rest space, then split into swing and twist; the twist is the
/// rotation about the axis and the swing is dropped. This is the
/// swing-twist decomposition of the rotation the spec describes as
/// `fromToQuat.inverse * deltaSrcQuatInDst`, without building the arc.
fn twist_about(axis: Vec3, target_rest: Quat, source_rest: Quat, source: Quat) -> Quat {
    let delta = source_rest.inverse() * source;
    let in_parent = source_rest * delta * source_rest.inverse();
    let in_target = target_rest.inverse() * in_parent * target_rest;
    let about_axis = axis.dot(in_target.xyz());
    let twist = Quat::from_xyzw(
        axis.x * about_axis,
        axis.y * about_axis,
        axis.z * about_axis,
        in_target.w,
    );
    let twist = if twist.length_squared() > f32::EPSILON {
        twist.normalize()
    } else {
        Quat::IDENTITY
    };
    target_rest * twist
}

/// The smallest rotation of the target's rest orientation that points `axis`
/// at `source` (both in world space), expressed as a local rotation.
fn point_at(
    parent_rotation: Quat,
    target_rest: Quat,
    axis: Vec3,
    source: Vec3,
    target: Vec3,
) -> Option<Quat> {
    // Express the direction in the target's rest space so the rotation is
    // applied around the rest pose, not the already constrained pose.
    let direction = (parent_rotation * target_rest).inverse() * (source - target);
    let direction = direction.try_normalize()?;
    Some(target_rest * Quat::from_rotation_arc(axis, direction))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constraints_copy_twist_and_aim_from_rest_frames_and_reject_cycles() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        crate::runtime::setup(&mut app);
        let source_rest = Quat::from_euler(EulerRot::XYZ, 0.2, -0.1, 0.4);
        let rest = Quat::from_euler(EulerRot::XYZ, 0.1, 0.5, -0.2);

        // A twist constraint keeps the rest axes and only accepts rotation
        // about the chosen axis. A half turn around an orthogonal axis is
        // dropped entirely.
        for axis in [
            schema::RollAxis::X,
            schema::RollAxis::Y,
            schema::RollAxis::Z,
        ] {
            let axis = axis.vector();
            let weight = 0.35;
            let swing = axis.any_orthonormal_vector();
            for (delta, expected) in [
                (
                    Quat::from_axis_angle(swing, 0.8) * Quat::from_axis_angle(axis, 0.6),
                    rest * Quat::from_axis_angle(axis, 0.6 * weight),
                ),
                (Quat::from_axis_angle(swing, std::f32::consts::PI), rest),
            ] {
                let source = app
                    .world_mut()
                    .spawn(Transform::from_rotation(
                        rest * delta * rest.inverse() * source_rest,
                    ))
                    .id();
                let target = app.world_mut().spawn(Transform::from_rotation(rest)).id();
                let rig = app
                    .world_mut()
                    .spawn(ConstraintRig(vec![Binding {
                        target,
                        source,
                        target_rest: rest,
                        source_rest,
                        rule: Rule::Twist(axis),
                        weight,
                    }]))
                    .id();
                for _ in 0..3 {
                    app.update();
                }
                assert!(
                    app.world()
                        .get::<Transform>(target)
                        .unwrap()
                        .rotation
                        .abs_diff_eq(expected, 1e-5)
                );
                app.world_mut().entity_mut(rig).despawn();
            }
        }

        let source = app
            .world_mut()
            .spawn(
                Transform::from_xyz(1., 0., 1.)
                    .with_rotation(source_rest * Quat::from_rotation_z(0.6)),
            )
            .id();
        let parent = app
            .world_mut()
            .spawn(Transform::from_rotation(Quat::from_rotation_y(0.4)))
            .id();
        let target = app
            .world_mut()
            .spawn((Transform::from_rotation(rest), ChildOf(parent)))
            .id();
        let nodes = HashMap::from_iter([(0, source), (1, parent), (2, target)]);
        let order = |defs: &[(usize, schema::NodeConstraint)], world: &mut World| {
            compile_constraints(defs, world, &nodes).map(|rig| {
                rig.0
                    .iter()
                    .map(|binding| {
                        defs.iter()
                            .position(|(index, _)| nodes[index] == binding.target)
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        };
        let copy = |source| schema::NodeConstraint {
            spec_version: "1.0".into(),
            constraint: schema::Constraint {
                rotation: Some(schema::Rotation {
                    source,
                    weight: Some(1.),
                }),
                roll: None,
                aim: None,
            },
        };
        let aim = || schema::NodeConstraint {
            spec_version: "1.0".into(),
            constraint: schema::Constraint {
                rotation: None,
                roll: None,
                aim: Some(schema::Aim {
                    source: 0,
                    aim_axis: schema::AimAxis::PositiveZ,
                    weight: Some(1.),
                }),
            },
        };
        // A binding on node 1 writes what node 2's binding reads, so 1 runs
        // first regardless of document order.
        assert_eq!(
            order(&[(2, copy(1)), (1, copy(0))], app.world_mut()).unwrap(),
            vec![1, 0]
        );
        // A dependency loop and a self reference are both rejected.
        assert!(order(&[(0, copy(1)), (1, copy(0))], app.world_mut()).is_err());
        assert!(order(&[(0, copy(0))], app.world_mut()).is_err());
        // Aim reads the ancestor world positions, so a constraint writing the
        // parent must run earlier.
        assert_eq!(
            order(&[(2, aim()), (1, copy(0))], app.world_mut()).unwrap(),
            vec![1, 0]
        );
        assert!(order(&[(2, aim()), (1, copy(2))], app.world_mut()).is_err());
        assert_eq!(
            order(&[(2, copy(0)), (1, copy(0))], app.world_mut())
                .unwrap()
                .len(),
            2
        );
        let both = schema::NodeConstraint {
            spec_version: "1.0".into(),
            constraint: schema::Constraint {
                rotation: Some(schema::Rotation {
                    source: 1,
                    weight: Some(1.),
                }),
                roll: Some(schema::Roll {
                    source: 0,
                    roll_axis: schema::RollAxis::X,
                    weight: Some(1.),
                }),
                aim: None,
            },
        };
        let error = match compile_constraints(&[(2, both)], app.world_mut(), &nodes) {
            Err(error) => error,
            Ok(_) => panic!("two constraint kinds must be rejected"),
        };
        assert!(error.to_string().contains("exactly one"));

        // Copy and aim share one plan: the copied rotation must be visible to
        // the aim target in the same frame.
        let rotated = app.world_mut().spawn(Transform::from_rotation(rest)).id();
        app.world_mut()
            .entity_mut(target)
            .insert(Transform::IDENTITY);
        app.world_mut().spawn(ConstraintRig(vec![
            Binding {
                target: rotated,
                source,
                target_rest: rest,
                source_rest,
                rule: Rule::Copy,
                weight: 0.35,
            },
            Binding {
                target,
                source,
                target_rest: Quat::IDENTITY,
                source_rest: Quat::IDENTITY,
                rule: Rule::PointAt(Vec3::Z),
                weight: 1.,
            },
        ]));
        for _ in 0..4 {
            app.update();
            assert!(
                app.world()
                    .get::<Transform>(rotated)
                    .unwrap()
                    .rotation
                    .abs_diff_eq(rest * Quat::from_rotation_z(0.21), 1e-5)
            );
            let global = app.world().get::<GlobalTransform>(target).unwrap();
            assert!(
                (global.rotation() * Vec3::Z).abs_diff_eq(Vec3::new(1., 0., 1.).normalize(), 1e-5)
            );
        }
    }
}
