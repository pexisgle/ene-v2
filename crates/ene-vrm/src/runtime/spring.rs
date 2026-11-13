//! `VRMC_springBone` (1.0) simulation.
//!
//! Each spring is a chain of joints. A joint is simulated as the position of
//! its tail (the child joint) pulled by three forces:
//!
//! - inertia from the previous position, damped by `dragForce`,
//! - a restoring force towards the tail's rest position, scaled by
//!   `stiffness`, and
//! - gravity, `gravityDir * gravityPower`.
//!
//! The result is projected onto the bone length and pushed out of any collider
//! registered for the chain, then the joint's local rotation is set so the
//! bone points at the new tail. The spec's reference solver integrates per
//! frame; this crate runs it on a fixed step so inertia and damping do not
//! depend on frame pacing.
//!
//! Reference: `specification/VRMC_springBone-1.0/README.md` in the VRM
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
use std::time::Duration;

/// VRM 1.0 spring parameters in model-space meters.
///
/// The solver scales `stiffness`, `gravity_power`, and `hit_radius` by the
/// rig root's world scale. Callers should not pre-scale these values.
#[derive(Component, Clone, Copy, Debug, PartialEq, Reflect)]
#[reflect(Component, Clone)]
pub struct SpringJointProps {
    pub stiffness: f32,
    pub gravity_power: f32,
    pub gravity_dir: Vec3,
    pub drag_force: f32,
    pub hit_radius: f32,
}

impl Default for SpringJointProps {
    fn default() -> Self {
        Self {
            stiffness: 1.,
            gravity_power: 0.,
            gravity_dir: Vec3::NEG_Y,
            drag_force: 0.5,
            hit_radius: 0.,
        }
    }
}

/// One simulated joint and the tail it swings.
#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Swing {
    #[entities]
    pub(crate) entity: Entity,
    /// Local transform of the joint in the bind pose.
    pub(crate) rest: Transform,
    /// Tail position in the joint's rest space.
    pub(crate) tail: Vec3,
    /// Current and previous tail positions, relative to the chain's center.
    pub(crate) motion: Option<[Vec3; 2]>,
    /// Rotation written by the last simulation step. Frames without a step
    /// restore it after earlier systems (constraints, for example) wrote to
    /// the same transform.
    pub(crate) applied: Option<Quat>,
}

/// A spring: its joints in hierarchy order, plus the colliders it samples.
#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Chain {
    #[entities]
    pub(crate) joints: Vec<Swing>,
    pub(crate) colliders: Vec<usize>,
    #[entities]
    pub(crate) center: Option<Entity>,
}

/// A sphere (two equal endpoints) or capsule the joints are pushed out of.
#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Collider {
    #[entities]
    pub(crate) entity: Entity,
    pub(crate) segment: Segment3d,
    pub(crate) radius: f32,
}

/// Spring state of one scene root.
#[derive(Component, Clone, Default, Reflect)]
#[reflect(Component, Clone)]
pub struct SpringRig {
    #[entities]
    pub(crate) chains: Vec<Chain>,
    #[entities]
    pub(crate) colliders: Vec<Collider>,
    /// When true, the solver keeps the last simulated pose instead of
    /// advancing. This is separate from removing [`SpringJointProps`], which
    /// opts a joint out of SpringBone entirely.
    pub paused: bool,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<SpringJointProps>()
        .register_type::<SpringRig>()
        .add_systems(PostUpdate, simulate.in_set(VrmSystems::SpringBone));
}

/// Builds the spring rig of one scene root from the extension definition.
pub(crate) fn attach(
    world: &mut World,
    root: Entity,
    springs: &schema::Springs,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<()> {
    let colliders = build_colliders(springs, nodes)?;
    let groups = springs.collider_groups.as_deref().unwrap_or_default();
    if groups.iter().any(|group| {
        group
            .colliders
            .iter()
            .any(|&index| index >= colliders.len())
    }) {
        return Err(invalid("VRMC collider out of range"));
    }
    let mut chains = Vec::new();
    for spring in springs.springs.as_deref().unwrap_or_default() {
        chains.push(build_chain(world, spring, groups, nodes)?);
    }
    // Chains cannot share joints, but a chain may start below another chain's
    // tail. Evaluate ancestors first so the child chain reads this frame's
    // parent pose instead of being one frame late.
    {
        let mut parent_query = world.query::<&ChildOf>();
        let parents = parent_query.query(world);
        chains.sort_by_key(|chain| {
            chain
                .joints
                .first()
                .map_or(0, |joint| parents.iter_ancestors(joint.entity).count())
        });
    }
    world.entity_mut(root).insert(SpringRig {
        chains,
        colliders,
        paused: false,
    });
    Ok(())
}

fn build_colliders(
    springs: &schema::Springs,
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<Vec<Collider>> {
    let mut colliders = Vec::new();
    for definition in springs.colliders.as_deref().unwrap_or_default() {
        let entity = require_node(nodes, schema::index(definition.node))?;
        let (points, radius) = match (&definition.shape.sphere, &definition.shape.capsule) {
            (Some(sphere), None) => ([scene_point(sphere.offset); 2], scene_scalar(sphere.radius)),
            (None, Some(capsule)) => (
                [scene_point(capsule.offset), scene_point(capsule.tail)],
                scene_scalar(capsule.radius),
            ),
            _ => {
                return Err(invalid(
                    "VRMC collider requires exactly one of sphere or capsule",
                ));
            }
        };
        if !radius.is_finite() || radius < 0. || !points[0].is_finite() || !points[1].is_finite() {
            return Err(invalid("VRMC collider shape is invalid"));
        }
        colliders.push(Collider {
            entity,
            segment: Segment3d::from(points),
            radius,
        });
    }
    Ok(colliders)
}

fn build_chain(
    world: &mut World,
    spring: &vrm_spec::vrmc_spring_bone_1_0::Spring,
    groups: &[vrm_spec::vrmc_spring_bone_1_0::ColliderGroup],
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<Chain> {
    if spring.joints.is_empty() {
        return Err(invalid("VRMC spring requires joints"));
    }
    let joints = spring
        .joints
        .iter()
        .map(|joint| {
            let entity = require_node(nodes, schema::index(joint.node))?;
            Ok((entity, joint_props(joint)))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    for (_, props) in &joints {
        if !props.gravity_dir.is_finite()
            || !props.gravity_power.is_finite()
            || props.gravity_power < 0.
            || !props.stiffness.is_finite()
            || props.stiffness < 0.
            || !(0. ..=1.).contains(&props.drag_force)
            || !props.hit_radius.is_finite()
            || props.hit_radius < 0.
        {
            return Err(invalid("VRMC invalid spring parameters"));
        }
    }
    let mut swings = Vec::with_capacity(joints.len().saturating_sub(1));
    for pair in joints.windows(2) {
        let (entity, props) = pair[0];
        let (&local, global) = world
            .entity(entity)
            .components::<(&Transform, &GlobalTransform)>();
        let tail = global.affine().inverse().transform_point3(
            world
                .entity(pair[1].0)
                .components::<&GlobalTransform>()
                .translation(),
        );
        if tail.length_squared() == 0. {
            continue;
        }
        world.entity_mut(entity).insert(props);
        swings.push(Swing {
            entity,
            rest: local,
            tail,
            motion: None,
            applied: None,
        });
    }
    let mut chain_colliders = Vec::new();
    for &group in spring.collider_groups.as_deref().unwrap_or_default() {
        let group =
            usize::try_from(group).map_err(|_| invalid("VRMC collider group out of range"))?;
        chain_colliders.extend_from_slice(
            &groups
                .get(group)
                .ok_or_else(|| invalid("VRMC collider group out of range"))?
                .colliders,
        );
    }
    let center = spring
        .center
        .map(|center| require_node(nodes, schema::index(center)))
        .transpose()?;
    Ok(Chain {
        joints: swings,
        colliders: chain_colliders,
        center,
    })
}

fn scene_point(value: Option<[f64; 3]>) -> Vec3 {
    value.map_or(Vec3::ZERO, |[x, y, z]| {
        Vec3::new(x as f32, y as f32, z as f32)
    })
}

fn scene_scalar(value: Option<f64>) -> f32 {
    value.unwrap_or(0.) as f32
}

fn joint_props(joint: &vrm_spec::vrmc_spring_bone_1_0::SpringBoneJoint) -> SpringJointProps {
    SpringJointProps {
        stiffness: joint.stiffness.unwrap_or(1.) as f32,
        gravity_power: joint.gravity_power.unwrap_or(0.) as f32,
        gravity_dir: joint.gravity_dir.map_or(Vec3::NEG_Y, |[x, y, z]| {
            Vec3::new(x as f32, y as f32, z as f32)
        }),
        drag_force: joint.drag_force.unwrap_or(0.5) as f32,
        hit_radius: joint.hit_radius.unwrap_or(0.) as f32,
    }
}

/// Fixed simulation step. The solver applies inertia and damping once per
/// step, so frame-time jitter must not change how many times they run.
const STEP: f32 = 1. / 60.;
/// [`STEP`] truncated to whole nanoseconds. [`Duration::from_secs_f32`] rounds
/// up to 16_666_668 ns and would drop a step at a 60 Hz frame delta.
const STEP_PERIOD: Duration = Duration::from_nanos(16_666_667);
/// Upper bound for time spent catching up after a long stall.
const MAX_STEPS: u32 = 15;

/// Advances a fixed-step clock and reports how many steps to simulate.
struct Stepper(Timer);

impl Default for Stepper {
    fn default() -> Self {
        Self(Timer::new(STEP_PERIOD, TimerMode::Repeating))
    }
}

impl Stepper {
    fn advance(&mut self, frame: Duration) -> usize {
        // Clamp the input, not the remainder, so a long stall drops the excess
        // time and still leaves less than one step for the next frame.
        let maximum = self.0.duration() * MAX_STEPS;
        self.0.tick(frame.min(maximum));
        self.0.times_finished_this_tick() as usize
    }
}

fn simulate(
    time: Res<Time>,
    mut stepper: Local<Stepper>,
    mut rigs: Query<(Entity, &mut SpringRig)>,
    props: Query<&SpringJointProps>,
    parents: Query<&ChildOf>,
    mut transforms: ParamSet<(TransformHelper, Query<&mut Transform>)>,
) {
    let steps = stepper.advance(time.delta());
    for (root, mut rig) in &mut rigs {
        if rig.paused || steps == 0 {
            // A system earlier this frame may have overwritten a joint pose
            // (node constraints, for example), so restore the visible one.
            restore_pose(&rig, &props, &mut transforms.p1());
            continue;
        }
        // Bevy propagates `GlobalTransform` after `VrmSystems::SpringBone`, so
        // read the rig root from `Transform` like every other node here.
        let Ok(root_transform) = transforms.p0().compute_global_transform(root) else {
            continue;
        };
        let meter = root_transform.scale().abs().max_element();
        let SpringRig {
            chains,
            colliders: definitions,
            ..
        } = &mut *rig;
        for _ in 0..steps {
            for chain in chains.iter_mut() {
                // A collider or center may be driven by a joint updated above,
                // so resolve them again for each step.
                let bounds: Vec<(Segment3d, f32)> = chain
                    .colliders
                    .iter()
                    .filter_map(|&index| {
                        let collider = &definitions[index];
                        let global = transforms
                            .p0()
                            .compute_global_transform(collider.entity)
                            .ok()?;
                        Some((
                            Segment3d::from(
                                collider
                                    .segment
                                    .vertices
                                    .map(|point| global.transform_point(point)),
                            ),
                            collider.radius * global.scale().abs().max_element(),
                        ))
                    })
                    .collect();
                let center = chain
                    .center
                    .and_then(|entity| transforms.p0().compute_global_transform(entity).ok())
                    .unwrap_or_default();
                let inverse_center = center.affine().inverse();
                for joint in &mut chain.joints {
                    let Ok(props) = props.get(joint.entity) else {
                        continue;
                    };
                    let parent = parents
                        .get(joint.entity)
                        .ok()
                        .and_then(|parent| {
                            transforms
                                .p0()
                                .compute_global_transform(parent.parent())
                                .ok()
                        })
                        .unwrap_or_default();
                    let posed = parent.mul_transform(joint.rest);
                    let head = posed.translation();
                    let length = posed.affine().transform_vector3(joint.tail).length();
                    let resting_tail = posed.transform_point(joint.tail);
                    let restoring =
                        (resting_tail - head).normalize_or_zero() * props.stiffness * meter * STEP;
                    let gravity = props.gravity_dir * props.gravity_power * meter * STEP;
                    let [current, previous] = joint.motion.map_or([resting_tail; 2], |motion| {
                        motion.map(|point| center.transform_point(point))
                    });
                    let mut next = head
                        + (current
                            + (current - previous) * (1. - props.drag_force)
                            + restoring
                            + gravity
                            - head)
                            .normalize_or_zero()
                            * length;
                    for (segment, radius) in &bounds {
                        push_out(
                            &mut next,
                            head,
                            length,
                            props.hit_radius * meter + radius,
                            segment,
                        );
                    }
                    joint.motion =
                        Some([next, current].map(|point| inverse_center.transform_point3(point)));
                    let rotation =
                        bend_towards(&posed, joint.rest.rotation, joint.tail, next, head);
                    joint.applied = Some(rotation);
                    if let Ok(mut transform) = transforms.p1().get_mut(joint.entity)
                        && transform.rotation != rotation
                    {
                        transform.rotation = rotation;
                    }
                }
            }
        }
    }
}

/// Restores the pose written by the last step without advancing the solver.
fn restore_pose(
    rig: &SpringRig,
    props: &Query<&SpringJointProps>,
    transforms: &mut Query<&mut Transform>,
) {
    for chain in &rig.chains {
        for joint in &chain.joints {
            if props.get(joint.entity).is_err() {
                continue;
            }
            let Some(rotation) = joint.applied else {
                continue;
            };
            if let Ok(mut transform) = transforms.get_mut(joint.entity)
                && transform.rotation != rotation
            {
                transform.rotation = rotation;
            }
        }
    }
}

/// Rotates the joint so its bone points at `tail` while keeping the rest
/// rotation as the reference.
fn bend_towards(
    posed: &GlobalTransform,
    local_rest: Quat,
    bone_tail: Vec3,
    tail: Vec3,
    head: Vec3,
) -> Quat {
    let direction = posed
        .affine()
        .inverse()
        .transform_vector3(tail - head)
        .normalize_or_zero();
    let bend = if direction == Vec3::ZERO {
        Quat::IDENTITY
    } else {
        Quat::from_rotation_arc(bone_tail.normalize_or_zero(), direction)
    };
    local_rest * bend
}

/// Pushes a tail out of a collider if it is closer than `clearance`, then
/// projects it back onto the bone length.
fn push_out(tail: &mut Vec3, head: Vec3, length: f32, clearance: f32, collider: &Segment3d) {
    let nearest = collider.closest_point(*tail);
    let offset = *tail - nearest;
    if offset.length_squared() >= clearance * clearance {
        return;
    }
    let away = offset
        .try_normalize()
        .or_else(|| (head - nearest).try_normalize())
        .unwrap_or(Vec3::Y);
    *tail = head + (nearest + away * clearance - head).normalize_or_zero() * length;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One joint hanging from a scaled root, optionally touching a sphere
    /// collider placed next to its tail.
    fn simulation(scale: f32, collision: bool) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin))
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                Duration::from_secs_f64(1. / 60.),
            ));
        crate::runtime::setup(&mut app);
        app.configure_sets(
            PostUpdate,
            crate::VrmSystems::SpringBone.before(bevy::transform::TransformSystems::Propagate),
        );
        let root = app
            .world_mut()
            .spawn(Transform::from_scale(Vec3::splat(scale)))
            .id();
        let rest = Transform::from_xyz(0., 1., 0.);
        let props = SpringJointProps {
            gravity_power: if collision { 0. } else { 1. },
            stiffness: if collision { 0. } else { 0.5 },
            hit_radius: 0.02,
            drag_force: if collision { 1. } else { 0.4 },
            ..default()
        };
        let joint = app.world_mut().spawn((rest, ChildOf(root), props)).id();
        let collider = app
            .world_mut()
            .spawn((Transform::from_xyz(0.1, 1.03, 0.), ChildOf(root)))
            .id();
        app.world_mut().entity_mut(root).insert(SpringRig {
            chains: vec![Chain {
                joints: vec![Swing {
                    entity: joint,
                    rest,
                    tail: Vec3::X * 0.1,
                    motion: None,
                    applied: None,
                }],
                colliders: if collision { vec![0] } else { vec![] },
                center: None,
            }],
            colliders: if collision {
                vec![Collider {
                    entity: collider,
                    segment: Segment3d::new(Vec3::ZERO, Vec3::ZERO),
                    radius: 0.02,
                }]
            } else {
                vec![]
            },
            paused: false,
        });
        app.update();
        (app, joint)
    }

    #[test]
    fn center_space_scale_and_rest_pose_invariants() {
        // Center space only changes where inertia is stored, not the pose.
        let (mut free, a) = simulation(1., false);
        let (mut centered, b) = simulation(1., false);
        let center_transform = Transform::from_xyz(5., 7., 11.)
            .with_rotation(Quat::from_rotation_z(0.4))
            .with_scale(Vec3::splat(2.));
        let center = centered.world_mut().spawn(center_transform).id();
        let root = centered.world().get::<ChildOf>(b).unwrap().parent();
        centered.world_mut().entity_mut(root).insert((
            GlobalTransform::IDENTITY.reparented_to(&GlobalTransform::from(center_transform)),
            ChildOf(center),
        ));
        centered
            .world_mut()
            .query::<&mut SpringRig>()
            .single_mut(centered.world_mut())
            .unwrap()
            .chains[0]
            .center = Some(center);
        for _ in 0..30 {
            free.update();
            centered.update();
        }
        assert!(
            free.world()
                .get::<Transform>(a)
                .unwrap()
                .rotation
                .abs_diff_eq(centered.world().get::<Transform>(b).unwrap().rotation, 1e-4)
        );

        // Without forces the simulated rotation stays at the (non-uniformly
        // scaled) rest rotation.
        let (mut posed, joint) = simulation(1., false);
        let root = posed.world().get::<ChildOf>(joint).unwrap().parent();
        posed.world_mut().get_mut::<Transform>(root).unwrap().scale = Vec3::new(2., 3., 4.);
        let rest = Transform::from_xyz(0., 1., 0.).with_rotation(Quat::from_rotation_y(0.6));
        posed.world_mut().entity_mut(joint).insert(rest);
        posed
            .world_mut()
            .query::<&mut SpringRig>()
            .single_mut(posed.world_mut())
            .unwrap()
            .chains[0]
            .joints[0]
            .rest = rest;
        posed
            .world_mut()
            .get_mut::<SpringJointProps>(joint)
            .unwrap()
            .gravity_power = 0.;
        for _ in 0..30 {
            posed.update();
        }
        assert!(
            posed
                .world()
                .get::<Transform>(joint)
                .unwrap()
                .rotation
                .abs_diff_eq(rest.rotation, 1e-5)
        );
    }

    #[test]
    fn collider_contact_and_finite_directions() {
        // A capsule behaves like the equivalent sphere at both ends and
        // inside; the projected tail always differs from the colliding one.
        let capsule = Segment3d::new(Vec3::ZERO, 2. * Vec3::Y);
        for (tail, sphere_center) in [
            (Vec3::new(0.1, 1., 0.), Vec3::Y),
            (Vec3::new(0.1, 2.1, 0.), 2. * Vec3::Y),
            (Vec3::new(0.1, -0.1, 0.), Vec3::ZERO),
        ] {
            let (mut actual, mut expected) = (tail, tail);
            let head = Vec3::ONE;
            let length = head.distance(tail);
            push_out(&mut actual, head, length, 0.22, &capsule);
            push_out(
                &mut expected,
                head,
                length,
                0.22,
                &Segment3d::new(sphere_center, sphere_center),
            );
            assert!(actual.abs_diff_eq(expected, 1e-5) && !actual.abs_diff_eq(tail, 1e-5));
        }
        let mut at_head = Vec3::ZERO;
        push_out(
            &mut at_head,
            Vec3::ZERO,
            160.,
            160.1,
            &Segment3d::new(Vec3::ZERO, Vec3::ZERO),
        );
        assert!(at_head.is_finite() && at_head.length() == 160.);

        // Spring parameters are authored in meters, so the same model at any
        // root scale produces the same pose.
        let (mut normal, normal_joint) = simulation(1., false);
        let (mut scaled, scaled_joint) = simulation(160., false);
        for _ in 0..30 {
            normal.update();
        }
        for _ in 0..60 {
            scaled.update();
        }
        let rotation = |app: &App, entity| app.world().get::<Transform>(entity).unwrap().rotation;
        assert!(
            rotation(&normal, normal_joint).angle_between(rotation(&scaled, scaled_joint)) < 0.001,
            "spec-meter spring parameters must be scale invariant"
        );

        // Hit radius and collider radius scale together with the model.
        let (mut unit, unit_joint) = simulation(1., true);
        let (mut large, large_joint) = simulation(160., true);
        unit.update();
        large.update();
        assert!(rotation(&unit, unit_joint).angle_between(Quat::IDENTITY) > 0.01);
        assert!(
            rotation(&unit, unit_joint).angle_between(rotation(&large, large_joint)) < 0.001,
            "collision radius and hit radius must scale together"
        );

        // A collapsed tail pair still yields a finite rotation, and gravity
        // from that state rotates the joint.
        let (mut collapsed, collapsed_joint) = simulation(1., false);
        collapsed
            .world_mut()
            .query::<&mut SpringRig>()
            .single_mut(collapsed.world_mut())
            .unwrap()
            .chains[0]
            .joints[0]
            .motion = Some([Vec3::Y; 2]);
        *collapsed
            .world_mut()
            .get_mut::<SpringJointProps>(collapsed_joint)
            .unwrap() = SpringJointProps {
            stiffness: 0.,
            gravity_power: 0.,
            ..default()
        };
        collapsed.update();
        assert!(rotation(&collapsed, collapsed_joint).is_finite());
        collapsed
            .world_mut()
            .get_mut::<SpringJointProps>(collapsed_joint)
            .unwrap()
            .gravity_power = 1.;
        collapsed.update();
        assert!(rotation(&collapsed, collapsed_joint).angle_between(Quat::IDENTITY) > 0.1);
    }
}
