// VRM constraint math adapted from bevy_vrm1, Copyright 2025 elm,
// under MIT OR Apache-2.0; see LICENSE-bevy_vrm1-*.
use crate::{
    VrmSystems,
    loader::{invalid, require_node},
    schema,
};
use bevy::{
    ecs::entity::MapEntities, platform::collections::HashMap, prelude::*,
    transform::helper::TransformHelper,
};

#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Constraint {
    #[entities]
    pub(crate) target: Entity,
    #[entities]
    pub(crate) source: Entity,
    pub(crate) rest: Quat,
    pub(crate) source_rest: Quat,
    pub(crate) kind: ConstraintKind,
    pub(crate) weight: f32,
}

#[derive(Clone, Copy, Reflect)]
pub(crate) enum ConstraintKind {
    Rotation,
    Roll(Vec3),
    Aim(Vec3),
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct Constraints(#[entities] pub(crate) Vec<Constraint>);

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Constraints>()
        .add_systems(PostUpdate, constraints.in_set(VrmSystems::Constraints));
}

pub(crate) fn compile_constraints(
    definitions: &[(usize, schema::NodeConstraint)],
    world: &mut World,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<Constraints> {
    let mut parent_query = world.query::<&ChildOf>();
    let parents = parent_query.query(world);
    let mut pending = Vec::new();
    for (target, def) in definitions {
        let (source, weight, kind) = match &def.constraint {
            schema::Constraint::Rotation(d) => {
                (d.source, d.weight.unwrap_or(1.), ConstraintKind::Rotation)
            }
            schema::Constraint::Roll(d) => (
                d.source,
                d.weight.unwrap_or(1.),
                ConstraintKind::Roll(d.roll_axis.vector()),
            ),
            schema::Constraint::Aim(d) => (
                d.source,
                d.weight.unwrap_or(1.),
                ConstraintKind::Aim(d.aim_axis.vector()),
            ),
        };
        let (target, source) = (require_node(nodes, *target)?, require_node(nodes, source)?);
        if !weight.is_finite() || !(0. ..=1.).contains(&weight) {
            return Err(invalid("VRMC constraint weight out of range"));
        }
        let mut reads = std::collections::HashSet::from([source]);
        if matches!(kind, ConstraintKind::Aim(_)) {
            for e in [source, target] {
                reads.extend(parents.iter_ancestors(e));
            }
        }
        pending.push((
            Constraint {
                target,
                source,
                rest: world.entity(target).components::<&Transform>().rotation,
                source_rest: world.entity(source).components::<&Transform>().rotation,
                kind,
                weight,
            },
            reads,
        ));
    }
    let mut remaining: std::collections::HashSet<_> =
        pending.iter().map(|(c, _)| c.target).collect();
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let next = pending
            .iter()
            .position(|(_, reads)| reads.is_disjoint(&remaining))
            .ok_or_else(|| invalid("VRMC constraint cycle"))?;
        let (c, _) = pending.remove(next);
        remaining.remove(&c.target);
        ordered.push(c);
    }
    Ok(Constraints(ordered))
}

fn constraints(
    rigs: Query<&Constraints>,
    parents: Query<&ChildOf>,
    mut transforms: ParamSet<(TransformHelper, Query<&mut Transform>)>,
) {
    for rig in &rigs {
        for c in &rig.0 {
            let src_local = transforms
                .p1()
                .get(c.source)
                .map_or(c.source_rest, |t| t.rotation);
            let rotation = match c.kind {
                ConstraintKind::Rotation => c.rest * c.source_rest.inverse() * src_local,
                ConstraintKind::Roll(axis) => {
                    let delta = c.rest.inverse() * src_local * c.source_rest.inverse() * c.rest;
                    let p = axis * Vec3::new(delta.x, delta.y, delta.z).dot(axis);
                    let twist = Quat::from_xyzw(p.x, p.y, p.z, delta.w);
                    c.rest
                        * if twist.length_squared() > f32::EPSILON {
                            twist.normalize()
                        } else {
                            Quat::IDENTITY
                        }
                }
                ConstraintKind::Aim(axis) => {
                    let (Ok(src), Ok(dst)) = (
                        transforms.p0().compute_global_transform(c.source),
                        transforms.p0().compute_global_transform(c.target),
                    ) else {
                        continue;
                    };
                    let parent_rot = parents
                        .get(c.target)
                        .ok()
                        .and_then(|p| transforms.p0().compute_global_transform(p.parent()).ok())
                        .map_or(Quat::IDENTITY, |p| p.rotation().normalize());
                    let dir = ((parent_rot * c.rest).inverse()
                        * (src.translation() - dst.translation()))
                    .normalize_or_zero();
                    if dir == Vec3::ZERO {
                        continue;
                    }
                    c.rest * Quat::from_rotation_arc(axis, dir)
                }
            };
            if let Ok(mut target) = transforms.p1().get_mut(c.target) {
                target.rotation = c.rest.slerp(rotation, c.weight);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_roll_and_aim_constraints_preserve_rest_axes_and_reject_cycles() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        crate::runtime::setup(&mut app);
        let source_rest = Quat::from_euler(EulerRot::XYZ, 0.2, -0.1, 0.4);
        let rest = Quat::from_euler(EulerRot::XYZ, 0.1, 0.5, -0.2);
        for axis in [
            schema::RollAxis::X,
            schema::RollAxis::Y,
            schema::RollAxis::Z,
        ] {
            let v = axis.vector();
            let w = 0.35;
            let swing = v.any_orthonormal_vector();
            for (delta, expected) in [
                (
                    Quat::from_axis_angle(swing, 0.8) * Quat::from_axis_angle(v, 0.6),
                    rest * Quat::from_axis_angle(v, 0.6 * w),
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
                    .spawn(Constraints(vec![Constraint {
                        target,
                        source,
                        rest,
                        source_rest,
                        kind: ConstraintKind::Roll(v),
                        weight: w,
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
            compile_constraints(defs, world, &nodes).map(|c| {
                c.0.iter()
                    .map(|x| defs.iter().position(|(i, _)| nodes[i] == x.target).unwrap())
                    .collect::<Vec<_>>()
            })
        };
        let rot = |s| schema::NodeConstraint {
            _version: schema::Version::V1,
            constraint: schema::Constraint::Rotation(schema::Rotation {
                source: s,
                weight: Some(1.),
            }),
        };
        let aim = || schema::NodeConstraint {
            _version: schema::Version::V1,
            constraint: schema::Constraint::Aim(schema::Aim {
                source: 0,
                aim_axis: schema::AimAxis::PositiveZ,
                weight: Some(1.),
            }),
        };
        assert_eq!(
            order(&[(2, rot(1)), (1, rot(0))], app.world_mut()).unwrap(),
            vec![1, 0]
        );
        assert!(order(&[(0, rot(1)), (1, rot(0))], app.world_mut()).is_err());
        assert!(order(&[(0, rot(0))], app.world_mut()).is_err());
        assert_eq!(
            order(&[(2, aim()), (1, rot(0))], app.world_mut()).unwrap(),
            vec![1, 0]
        );
        assert!(order(&[(2, aim()), (1, rot(2))], app.world_mut()).is_err());

        let rot_target = app.world_mut().spawn(Transform::from_rotation(rest)).id();
        app.world_mut()
            .entity_mut(target)
            .insert(Transform::IDENTITY);
        app.world_mut().spawn(Constraints(vec![
            Constraint {
                target: rot_target,
                source,
                rest,
                source_rest,
                kind: ConstraintKind::Rotation,
                weight: 0.35,
            },
            Constraint {
                target,
                source,
                rest: Quat::IDENTITY,
                source_rest: Quat::IDENTITY,
                kind: ConstraintKind::Aim(Vec3::Z),
                weight: 1.,
            },
        ]));
        for _ in 0..4 {
            app.update();
            assert!(
                app.world()
                    .get::<Transform>(rot_target)
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
