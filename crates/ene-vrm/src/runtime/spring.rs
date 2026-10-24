// VRM spring math adapted from bevy_vrm1, Copyright 2025 elm,
// under MIT OR Apache-2.0; see LICENSE-bevy_vrm1-*.
use crate::VrmSystems;
use bevy::{ecs::entity::MapEntities, prelude::*, transform::helper::TransformHelper};
use serde::Deserialize;

/// VRM 1.0 spring parameters in model-space meters.
///
/// The solver scales `stiffness`, `gravity_power`, and `hit_radius` by the
/// rig root's world scale. Callers should not pre-scale these values.
#[derive(Component, Clone, Copy, Debug, PartialEq, Deserialize, Reflect)]
#[reflect(Component, Clone)]
#[serde(default, rename_all = "camelCase")]
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

#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct SpringJoint {
    #[entities]
    pub(crate) entity: Entity,
    pub(crate) rest: Transform,
    pub(crate) tail: Vec3,
    pub(crate) state: Option<[Vec3; 2]>,
    /// Last simulated local rotation, kept so frames without a physics step
    /// can restore the pose after earlier systems (for example constraints)
    /// wrote to the same Transform.
    pub(crate) rotation: Option<Quat>,
}

#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct Collider {
    #[entities]
    pub(crate) entity: Entity,
    pub(crate) segment: Segment3d,
    pub(crate) radius: f32,
}

#[derive(Clone, Reflect, MapEntities)]
pub(crate) struct SpringChain {
    #[entities]
    pub(crate) joints: Vec<SpringJoint>,
    pub(crate) colliders: Vec<usize>,
    #[entities]
    pub(crate) center: Option<Entity>,
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct SpringRig {
    #[entities]
    pub(crate) chains: Vec<SpringChain>,
    #[entities]
    pub(crate) colliders: Vec<Collider>,
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<SpringJointProps>()
        .register_type::<SpringRig>()
        .add_systems(PostUpdate, springs.in_set(VrmSystems::SpringBone));
}

/// Reference solver step. The simulation runs on a fixed clock so frame-time
/// jitter does not change how many times inertia and damping are applied.
const STEP: f32 = 1. / 60.;
/// Upper bound for time spent catching up after a long stall.
const MAX_STEPS: usize = 15;

/// Accumulates frame time and returns how many fixed [`STEP`]s to simulate.
fn fixed_steps(accumulator: &mut f32, dt: f32) -> usize {
    // Keep half a step of slack so the capped value does not truncate to one
    // step less through float rounding.
    let max = STEP * (MAX_STEPS as f32 + 0.5);
    *accumulator = (*accumulator + dt).min(max);
    let steps = ((*accumulator / STEP) as usize).min(MAX_STEPS);
    *accumulator = (*accumulator - steps as f32 * STEP).max(0.);
    steps
}

fn springs(
    time: Res<Time>,
    mut accumulator: Local<f32>,
    mut rigs: Query<(Entity, &mut SpringRig)>,
    props: Query<&SpringJointProps>,
    parents: Query<&ChildOf>,
    mut transforms: ParamSet<(TransformHelper, Query<&mut Transform>)>,
) {
    let steps = fixed_steps(&mut accumulator, time.delta_secs());
    for (root_entity, mut rig) in &mut rigs {
        if steps == 0 {
            // Nothing to simulate, but a system earlier this frame (for
            // example node constraints) may have overwritten a joint pose.
            let mut query = transforms.p1();
            apply_spring_poses(&rig, &props, &mut query);
            continue;
        }
        // Bevy propagates `GlobalTransform` after `VrmSystems::SpringBone`, so
        // read the rig root from `Transform` like every other node here.
        let Ok(root) = transforms.p0().compute_global_transform(root_entity) else {
            continue;
        };
        let unit_scale = root.scale().abs().max_element();
        let SpringRig {
            chains,
            colliders: defs,
        } = &mut *rig;
        // Step the whole rig before the next step: a child joint must see the
        // pose its parent has at the start of the same step, not the pose after
        // every substep has run.
        for _ in 0..steps {
            for chain in chains.iter_mut() {
                // A collider or center may be driven by a joint updated above,
                // so recompute them for each step.
                let colliders: Vec<_> = chain
                    .colliders
                    .iter()
                    .filter_map(|&i| {
                        let c = &defs[i];
                        let g = transforms.p0().compute_global_transform(c.entity).ok()?;
                        Some((
                            Segment3d::from(c.segment.vertices.map(|p| g.transform_point(p))),
                            c.radius * g.scale().abs().max_element(),
                        ))
                    })
                    .collect();
                let center = chain
                    .center
                    .and_then(|e| transforms.p0().compute_global_transform(e).ok())
                    .unwrap_or_default();
                let inv_center = center.affine().inverse();
                for joint in &mut chain.joints {
                    let Ok(props) = props.get(joint.entity) else {
                        continue;
                    };
                    let parent = parents
                        .get(joint.entity)
                        .ok()
                        .and_then(|p| transforms.p0().compute_global_transform(p.parent()).ok())
                        .unwrap_or_default();
                    let rest = parent.mul_transform(joint.rest);
                    let head = rest.translation();
                    let length = rest.affine().transform_vector3(joint.tail).length();
                    let initial = rest.transform_point(joint.tail);
                    let stiffness =
                        (initial - head).normalize_or_zero() * props.stiffness * unit_scale * STEP;
                    let [current, previous] = joint
                        .state
                        .map_or([initial; 2], |s| s.map(|p| center.transform_point(p)));
                    let mut next = head
                        + (current
                            + (current - previous) * (1. - props.drag_force)
                            + stiffness
                            + props.gravity_dir * props.gravity_power * unit_scale * STEP
                            - head)
                            .normalize_or_zero()
                            * length;
                    for (segment, radius) in &colliders {
                        collide(
                            &mut next,
                            head,
                            length,
                            props.hit_radius * unit_scale + radius,
                            segment,
                        );
                    }
                    joint.state = Some([next, current].map(|p| inv_center.transform_point3(p)));
                    let dir = rest
                        .affine()
                        .inverse()
                        .transform_vector3(next - head)
                        .normalize_or_zero();
                    let rotation = joint.rest.rotation
                        * if dir != Vec3::ZERO {
                            Quat::from_rotation_arc(joint.tail.normalize_or_zero(), dir)
                        } else {
                            Quat::IDENTITY
                        };
                    joint.rotation = Some(rotation);
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

/// Applies the last simulated pose when a frame does not advance the solver.
fn apply_spring_poses(
    rig: &SpringRig,
    props: &Query<&SpringJointProps>,
    transforms: &mut Query<&mut Transform>,
) {
    for chain in &rig.chains {
        for joint in &chain.joints {
            if props.get(joint.entity).is_err() {
                continue;
            }
            let Some(rotation) = joint.rotation else {
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

fn collide(tail: &mut Vec3, head: Vec3, length: f32, radius: f32, segment: &Segment3d) {
    let center = segment.closest_point(*tail);
    let delta = *tail - center;
    if delta.length_squared() < radius * radius {
        let dir = delta
            .try_normalize()
            .or_else(|| (head - center).try_normalize())
            .unwrap_or(Vec3::Y);
        *tail = head + (center + dir * radius - head).normalize_or_zero() * length;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Vrm;
    use std::time::Duration;

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
            chains: vec![SpringChain {
                joints: vec![SpringJoint {
                    entity: joint,
                    rest,
                    tail: Vec3::X * 0.1,
                    state: None,
                    rotation: None,
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
        });
        app.update();
        (app, joint)
    }

    #[test]
    fn native_scene_instances_remap_one_shared_collider_for_multiple_chains() {
        use crate::tests::{Files, app, settle};
        let files = Files::new();
        let mut value = files.model();
        let base = value["nodes"].as_array().unwrap().len();
        value["nodes"][1]["children"] = serde_json::json!([2, 4, 7, base, base + 2]);
        value["nodes"].as_array_mut().unwrap().extend([
            serde_json::json!({"translation":[0.1,1.,0.],"children":[base + 1]}),
            serde_json::json!({"translation":[0.,0.1,0.]}),
            serde_json::json!({"translation":[-0.1,1.,0.],"children":[base + 3]}),
            serde_json::json!({"translation":[0.,0.1,0.]}),
        ]);
        value["extensions"]["VRMC_springBone"] = serde_json::json!({
            "specVersion":"1.0",
            "colliders":[{"node":1,"shape":{"sphere":{"offset":[0.,10.,0.],"radius":0.01}}}],
            "colliderGroups":[{"colliders":[0]}],
            "springs":[
                {"joints":[{"node":base},{"node":base + 1}],"colliderGroups":[0]},
                {"joints":[{"node":base + 2},{"node":base + 3}],"colliderGroups":[0]}
            ]
        });
        let mut app = app();
        let handle: Handle<bevy::gltf::Gltf> = app
            .world()
            .resource::<AssetServer>()
            .load(files.write("shared-collider.vrm", &value));
        settle(&mut app, std::slice::from_ref(&handle));
        let scene = app
            .world()
            .resource::<Assets<bevy::gltf::Gltf>>()
            .get(&handle)
            .unwrap()
            .default_scene
            .clone()
            .unwrap();
        let holders: Vec<_> = [0., 20.]
            .into_iter()
            .map(|x| {
                app.world_mut()
                    .spawn((
                        WorldAssetRoot(scene.clone()),
                        Transform::from_xyz(x, 0., 0.),
                    ))
                    .id()
            })
            .collect();
        for _ in 0..5 {
            app.update();
        }
        let mut q = app.world_mut().query::<(&Vrm, &SpringRig)>();
        let rigs: Vec<_> = q.iter(app.world()).collect();
        assert_eq!(rigs.len(), 2);
        for (vrm, rig) in &rigs {
            assert_eq!(rig.colliders[0].entity, vrm.bone("hips").unwrap());
            assert_eq!(rig.chains.len(), 2);
            for chain in &rig.chains {
                assert_eq!(chain.colliders, vec![0]);
                assert!(chain.joints[0].state.unwrap().iter().all(|p| p.is_finite()));
                assert!(
                    app.world()
                        .get::<SpringJointProps>(chain.joints[0].entity)
                        .is_some()
                );
            }
        }
        assert_ne!(
            rigs[0].1.chains[0].joints[0].entity,
            rigs[1].1.chains[0].joints[0].entity
        );
        app.world_mut().entity_mut(holders[0]).despawn();
        for _ in 0..3 {
            app.update();
        }
        let mut q = app.world_mut().query::<&SpringRig>();
        let rig = q.single(app.world()).unwrap();
        assert!(rig.chains[0].joints[0].state.is_some());
    }

    #[test]
    fn gravity_dir_scale_matches_gravity_power_scale() {
        let (mut by_dir, dir_joint) = simulation(1., false);
        let (mut by_power, power_joint) = simulation(1., false);
        by_dir
            .world_mut()
            .get_mut::<SpringJointProps>(dir_joint)
            .unwrap()
            .gravity_dir = Vec3::NEG_Y * 2.;
        by_power
            .world_mut()
            .get_mut::<SpringJointProps>(power_joint)
            .unwrap()
            .gravity_power = 2.;
        for _ in 0..30 {
            by_dir.update();
            by_power.update();
        }
        let a = by_dir.world().get::<Transform>(dir_joint).unwrap().rotation;
        let b = by_power
            .world()
            .get::<Transform>(power_joint)
            .unwrap()
            .rotation;
        assert!(a.abs_diff_eq(b, 1e-5));
    }

    #[test]
    fn spring_center_scale_and_collision_invariants() {
        let (mut normal, a) = simulation(1., false);
        let (mut centered, b) = simulation(1., false);
        let center_tf = Transform::from_xyz(5., 7., 11.)
            .with_rotation(Quat::from_rotation_z(0.4))
            .with_scale(Vec3::splat(2.));
        let center = centered.world_mut().spawn(center_tf).id();
        let root = centered.world().get::<ChildOf>(b).unwrap().parent();
        centered.world_mut().entity_mut(root).insert((
            GlobalTransform::IDENTITY.reparented_to(&GlobalTransform::from(center_tf)),
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
            normal.update();
            centered.update();
        }
        assert!(
            normal
                .world()
                .get::<Transform>(a)
                .unwrap()
                .rotation
                .abs_diff_eq(centered.world().get::<Transform>(b).unwrap().rotation, 1e-4)
        );

        // Non-uniform scale rest pose preservation
        let (mut non_uni, joint) = simulation(1., false);
        let root = non_uni.world().get::<ChildOf>(joint).unwrap().parent();
        non_uni
            .world_mut()
            .get_mut::<Transform>(root)
            .unwrap()
            .scale = Vec3::new(2., 3., 4.);
        let rest = Transform::from_xyz(0., 1., 0.).with_rotation(Quat::from_rotation_y(0.6));
        non_uni.world_mut().entity_mut(joint).insert(rest);
        non_uni
            .world_mut()
            .query::<&mut SpringRig>()
            .single_mut(non_uni.world_mut())
            .unwrap()
            .chains[0]
            .joints[0]
            .rest = rest;
        non_uni
            .world_mut()
            .get_mut::<SpringJointProps>(joint)
            .unwrap()
            .gravity_power = 0.;
        for _ in 0..30 {
            non_uni.update();
        }
        assert!(
            non_uni
                .world()
                .get::<Transform>(joint)
                .unwrap()
                .rotation
                .abs_diff_eq(rest.rotation, 1e-5)
        );

        // Capsule interior & endpoints + zero collision finite
        let capsule = Segment3d::new(Vec3::ZERO, 2. * Vec3::Y);
        for (tail, c) in [
            (Vec3::new(0.1, 1., 0.), Vec3::Y),
            (Vec3::new(0.1, 2.1, 0.), 2. * Vec3::Y),
            (Vec3::new(0.1, -0.1, 0.), Vec3::ZERO),
        ] {
            let (mut actual, mut expected) = (tail, tail);
            let head = Vec3::ONE;
            let len = head.distance(tail);
            collide(&mut actual, head, len, 0.22, &capsule);
            collide(&mut expected, head, len, 0.22, &Segment3d::new(c, c));
            assert!(actual.abs_diff_eq(expected, 1e-5) && !actual.abs_diff_eq(tail, 1e-5));
        }
        let mut zero_tail = Vec3::ZERO;
        collide(
            &mut zero_tail,
            Vec3::ZERO,
            160.,
            160.1,
            &Segment3d::new(Vec3::ZERO, Vec3::ZERO),
        );
        assert!(zero_tail.is_finite() && zero_tail.length() == 160.);

        // Unit scaling and collision radius behavior
        let (mut scaled, s_joint) = simulation(160., false);
        for _ in 0..30 {
            normal.update();
        }
        for _ in 0..60 {
            scaled.update();
        }
        let rot = |app: &App, e| app.world().get::<Transform>(e).unwrap().rotation;
        assert!(
            rot(&normal, a).angle_between(rot(&scaled, s_joint)) < 0.001,
            "spec-meter spring parameters must be scale invariant"
        );

        let (mut col_1, ca) = simulation(1., true);
        let (mut col_160, cb) = simulation(160., true);
        col_1.update();
        col_160.update();
        assert!(rot(&col_1, ca).angle_between(Quat::IDENTITY) > 0.01);
        assert!(
            rot(&col_1, ca).angle_between(rot(&col_160, cb)) < 0.001,
            "collision radius and hit radius must scale together"
        );

        // Collapsed direction recovery
        let (mut collapsed, cj) = simulation(1., false);
        collapsed
            .world_mut()
            .query::<&mut SpringRig>()
            .single_mut(collapsed.world_mut())
            .unwrap()
            .chains[0]
            .joints[0]
            .state = Some([Vec3::Y; 2]);
        *collapsed
            .world_mut()
            .get_mut::<SpringJointProps>(cj)
            .unwrap() = SpringJointProps {
            stiffness: 0.,
            gravity_power: 0.,
            ..default()
        };
        collapsed.update();
        assert!(rot(&collapsed, cj).is_finite());
        collapsed
            .world_mut()
            .get_mut::<SpringJointProps>(cj)
            .unwrap()
            .gravity_power = 1.;
        collapsed.update();
        assert!(rot(&collapsed, cj).angle_between(Quat::IDENTITY) > 0.1);
    }

    #[test]
    fn springbone_reads_the_current_root_scale_before_propagation() {
        let (mut stale, stale_joint) = simulation(1., false);
        let (mut propagated, propagated_joint) = simulation(1., false);
        for _ in 0..10 {
            stale.update();
            propagated.update();
        }
        let stale_root = stale.world().get::<ChildOf>(stale_joint).unwrap().parent();
        let propagated_root = propagated
            .world()
            .get::<ChildOf>(propagated_joint)
            .unwrap()
            .parent();
        stale
            .world_mut()
            .get_mut::<Transform>(stale_root)
            .unwrap()
            .scale = Vec3::splat(2.);
        propagated
            .world_mut()
            .get_mut::<Transform>(propagated_root)
            .unwrap()
            .scale = Vec3::splat(2.);
        // Simulate the frame in which propagation already ran for one app.
        propagated
            .world_mut()
            .entity_mut(propagated_root)
            .insert(GlobalTransform::from(Transform::from_scale(Vec3::splat(
                2.,
            ))));
        stale.update();
        propagated.update();
        let a = stale
            .world()
            .get::<Transform>(stale_joint)
            .unwrap()
            .rotation;
        let b = propagated
            .world()
            .get::<Transform>(propagated_joint)
            .unwrap()
            .rotation;
        assert!(a.abs_diff_eq(b, 1e-5), "{a:?} != {b:?}");
    }

    /// Builds a two-segment chain so a child joint can observe its parent.
    fn chain(dt: Duration) -> (App, [Entity; 2]) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin))
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(dt));
        crate::runtime::setup(&mut app);
        app.configure_sets(
            PostUpdate,
            crate::VrmSystems::SpringBone.before(bevy::transform::TransformSystems::Propagate),
        );
        let root = app.world_mut().spawn(Transform::default()).id();
        let props = SpringJointProps {
            gravity_power: 1.,
            stiffness: 0.5,
            drag_force: 0.4,
            ..default()
        };
        let parent_rest = Transform::from_xyz(0., 1., 0.);
        // Horizontal segments so gravity applies torque to both joints.
        let child_rest = Transform::from_xyz(0.1, 0., 0.);
        let parent = app
            .world_mut()
            .spawn((parent_rest, ChildOf(root), props))
            .id();
        let child = app
            .world_mut()
            .spawn((child_rest, ChildOf(parent), props))
            .id();
        app.world_mut().entity_mut(root).insert(SpringRig {
            chains: vec![SpringChain {
                joints: vec![
                    SpringJoint {
                        entity: parent,
                        rest: parent_rest,
                        tail: Vec3::X * 0.1,
                        state: None,
                        rotation: None,
                    },
                    SpringJoint {
                        entity: child,
                        rest: child_rest,
                        tail: Vec3::X * 0.1,
                        state: None,
                        rotation: None,
                    },
                ],
                colliders: vec![],
                center: None,
            }],
            colliders: vec![],
        });
        // Consume the first-update zero delta.
        app.update();
        (app, [parent, child])
    }

    #[test]
    fn springbone_steps_children_with_the_current_parent_pose() {
        let step = Duration::from_secs_f64(1. / 60.);
        // 6.25 steps in one frame: exactly six steps and a carried remainder.
        let (mut low, low_joints) = chain(Duration::from_secs_f64(1. / 60. * 6.25));
        let (mut high, high_joints) = chain(step);
        low.update();
        for _ in 0..6 {
            high.update();
        }
        for (low_joint, high_joint) in low_joints.into_iter().zip(high_joints) {
            let a = low.world().get::<Transform>(low_joint).unwrap().rotation;
            let b = high.world().get::<Transform>(high_joint).unwrap().rotation;
            assert!(a.abs_diff_eq(b, 1e-5), "{a:?} != {b:?}");
        }
    }

    #[test]
    fn springbone_restores_its_pose_on_frames_without_a_physics_step() {
        use crate::runtime::constraint::{Constraint, ConstraintKind, Constraints};

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin))
            .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
                Duration::from_secs_f64(1. / 120.),
            ));
        crate::runtime::setup(&mut app);
        app.configure_sets(
            PostUpdate,
            (
                crate::VrmSystems::Constraints.before(crate::VrmSystems::SpringBone),
                crate::VrmSystems::SpringBone.before(bevy::transform::TransformSystems::Propagate),
            ),
        );
        let root = app.world_mut().spawn(Transform::default()).id();
        let source = app
            .world_mut()
            .spawn((Transform::default(), ChildOf(root)))
            .id();
        let rest = Transform::from_xyz(0., 1., 0.);
        let joint = app
            .world_mut()
            .spawn((
                rest,
                ChildOf(root),
                SpringJointProps {
                    gravity_power: 1.,
                    stiffness: 0.5,
                    drag_force: 0.4,
                    ..default()
                },
            ))
            .id();
        app.world_mut().entity_mut(root).insert((
            SpringRig {
                chains: vec![SpringChain {
                    joints: vec![SpringJoint {
                        entity: joint,
                        rest,
                        tail: Vec3::X * 0.1,
                        state: None,
                        rotation: None,
                    }],
                    colliders: vec![],
                    center: None,
                }],
                colliders: vec![],
            },
            Constraints(vec![Constraint {
                target: joint,
                source,
                rest: Quat::IDENTITY,
                source_rest: Quat::IDENTITY,
                kind: ConstraintKind::Rotation,
                weight: 1.,
            }]),
        ));
        app.update(); // consume the first-update zero delta
        let mut simulated = Quat::IDENTITY;
        for _ in 0..6 {
            app.update();
            let rotation = app.world().get::<Transform>(joint).unwrap().rotation;
            if rotation.angle_between(Quat::IDENTITY) > 0.01 {
                simulated = rotation;
                break;
            }
        }
        assert!(
            simulated.angle_between(Quat::IDENTITY) > 0.01,
            "spring joint did not move"
        );
        // The constraint rewrites the transform every frame. A frame without a
        // physics step must restore the simulated pose instead of keeping it.
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_nanos(1),
        ));
        app.update();
        let restored = app.world().get::<Transform>(joint).unwrap().rotation;
        assert!(
            restored.abs_diff_eq(simulated, 1e-5),
            "{restored:?} != {simulated:?}"
        );
    }

    #[test]
    fn fixed_step_accumulator_does_not_double_steps_at_the_frame_boundary() {
        for dt in [1. / 60., 0.01666, 0.01667, 0.1] {
            let mut accumulator = 0.;
            let frames = 600;
            let mut steps = 0;
            for _ in 0..frames {
                steps += fixed_steps(&mut accumulator, dt);
            }
            let expected = (frames as f32 * dt / STEP) as usize;
            assert!(
                steps.abs_diff(expected) <= 1,
                "dt {dt}: {steps} steps, expected {expected}"
            );
            assert!(
                (0. ..STEP).contains(&accumulator),
                "remainder {accumulator}"
            );
        }
        let mut accumulator = 0.;
        assert_eq!(fixed_steps(&mut accumulator, 10.), MAX_STEPS);
        assert!(accumulator < STEP);
    }
}
