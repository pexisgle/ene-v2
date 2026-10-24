pub(crate) mod constraint;
pub(crate) mod look_at;
pub(crate) mod spring;

pub use look_at::LookAt;
pub use spring::{SpringJointProps, SpringRig};

use crate::{
    animation::{self, Pose, RestPose},
    expressions,
    loader::{invalid, require_node},
    schema,
};
use bevy::{
    animation::AnimatedBy, ecs::entity::MapEntities, platform::collections::HashMap, prelude::*,
};

#[derive(Clone, Reflect, MapEntities)]
pub struct HumanoidBone {
    pub name: String,
    #[entities]
    pub entity: Entity,
}

/// Metadata lives on Bevy's actual scene root. Entity references remap per instance.
#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct Vrm {
    #[entities]
    pub bones: Vec<HumanoidBone>,
    pub meta: schema::VrmMeta,
}

impl Vrm {
    pub fn bone(&self, name: &str) -> Option<Entity> {
        self.bones.iter().find(|b| b.name == name).map(|b| b.entity)
    }
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Vrm>()
        .register_type::<schema::VrmMeta>();
    look_at::setup(app);
    constraint::setup(app);
    spring::setup(app);
}

fn depth(parents: &HashMap<Entity, Entity>, mut entity: Entity) -> usize {
    let mut result = 0;
    while let Some(&parent) = parents.get(&entity) {
        result += 1;
        entity = parent;
    }
    result
}

pub(crate) fn attach(
    world: &mut World,
    root: Entity,
    model: &schema::Model,
    springs: Option<&schema::Springs>,
    constraints: &[(usize, schema::NodeConstraint)],
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<()> {
    let mut parent_query = world.query::<(Entity, &ChildOf)>();
    let parents: HashMap<Entity, Entity> = parent_query
        .iter(world)
        .map(|(entity, child_of)| (entity, child_of.parent()))
        .collect();
    let constraints = constraint::compile_constraints(constraints, world, nodes)?;
    let root_inv = world
        .get::<GlobalTransform>(root)
        .expect("native scene transform")
        .affine()
        .inverse();
    let height = model.humanoid.hips_height(world, nodes, root_inv);
    let mut bones = Vec::new();
    for (name, node) in &model.humanoid.human_bones {
        let entity = require_node(nodes, node.node)?;
        let (&local, global) = world
            .entity(entity)
            .components::<(&Transform, &GlobalTransform)>();
        let global = GlobalTransform::from(root_inv * global.affine());
        world.entity_mut(entity).insert(RestPose {
            local,
            global_rotation: global.rotation().normalize(),
            translation_from_normalized: (local.compute_affine() * global.affine().inverse())
                .matrix3
                * height,
        });
        world.spawn((
            Pose::default(),
            animation::bone_target(name),
            AnimatedBy(root),
            ChildOf(entity),
        ));
        bones.push(HumanoidBone {
            name: name.clone(),
            entity,
        });
    }
    if let Some(definition) = &model.look_at {
        let input = world
            .spawn((
                Transform::default(),
                animation::gaze_target(),
                AnimatedBy(root),
                ChildOf(root),
            ))
            .id();
        world.entity_mut(root).insert((
            LookAt::default(),
            look_at::Gaze {
                definition: definition.clone(),
                input,
                active: false,
            },
        ));
    }
    world.entity_mut(root).insert((
        Vrm {
            bones,
            meta: model.meta.clone(),
        },
        AnimationPlayer::default(),
        constraints,
    ));
    expressions::attach(world, root, &model.expressions, nodes)?;
    if let Some(springs) = springs {
        let colliders = springs
            .colliders
            .iter()
            .map(|c| {
                let entity = require_node(nodes, c.node)?;
                let (pts, radius) = match &c.shape {
                    schema::Shape::Sphere { offset, radius } => ([*offset; 2], *radius),
                    schema::Shape::Capsule {
                        offset,
                        tail,
                        radius,
                    } => ([*offset, *tail], *radius),
                };
                if !radius.is_finite() || radius < 0. || !pts[0].is_finite() || !pts[1].is_finite()
                {
                    return Err(invalid("VRMC collider shape is invalid"));
                }
                Ok(spring::Collider {
                    entity,
                    segment: Segment3d::from(pts),
                    radius,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        if springs
            .collider_groups
            .iter()
            .any(|g| g.colliders.iter().any(|&i| i >= colliders.len()))
        {
            return Err(invalid("VRMC collider out of range"));
        }
        let mut chains = Vec::new();
        for spring_def in &springs.springs {
            if spring_def.joints.is_empty() {
                return Err(invalid("VRMC spring requires joints"));
            }
            let mut joint_entities = Vec::with_capacity(spring_def.joints.len());
            for joint in &spring_def.joints {
                let entity = require_node(nodes, joint.node)?;
                let p = &joint.props;
                if !p.gravity_dir.is_finite()
                    || !p.gravity_power.is_finite()
                    || p.gravity_power < 0.
                    || !p.stiffness.is_finite()
                    || p.stiffness < 0.
                    || !(0. ..=1.).contains(&p.drag_force)
                    || !p.hit_radius.is_finite()
                    || p.hit_radius < 0.
                {
                    return Err(invalid("VRMC invalid spring parameters"));
                }
                joint_entities.push(entity);
            }
            let mut joints = Vec::new();
            for (i, pair) in joint_entities.windows(2).enumerate() {
                let (entity, next) = (pair[0], pair[1]);
                let (&local, global) = world
                    .entity(entity)
                    .components::<(&Transform, &GlobalTransform)>();
                let tail = global.affine().inverse().transform_point3(
                    world
                        .entity(next)
                        .components::<&GlobalTransform>()
                        .translation(),
                );
                if tail.length_squared() == 0. {
                    continue;
                }
                world.entity_mut(entity).insert(spring_def.joints[i].props);
                joints.push(spring::SpringJoint {
                    entity,
                    rest: local,
                    tail,
                    state: None,
                });
            }
            let mut chain_colliders = Vec::new();
            for &g in &spring_def.collider_groups {
                chain_colliders.extend_from_slice(
                    &springs
                        .collider_groups
                        .get(g)
                        .ok_or_else(|| invalid("VRMC collider group out of range"))?
                        .colliders,
                );
            }
            chains.push(spring::SpringChain {
                joints,
                colliders: chain_colliders,
                center: spring_def
                    .center
                    .map(|i| require_node(nodes, i))
                    .transpose()?,
            });
        }
        // Chains cannot share joints, but a chain may start below another
        // chain's tail. Evaluate ancestors first so the child chain reads this
        // frame's parent pose instead of being one frame late.
        chains.sort_by_key(|chain| {
            chain
                .joints
                .first()
                .map_or(0, |joint| depth(&parents, joint.entity))
        });
        world
            .entity_mut(root)
            .insert(spring::SpringRig { chains, colliders });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VrmSystems;

    #[test]
    fn vrm1_springs_and_gaze_expression_follow_spec() {
        use bevy::ecs::system::RunSystemOnce;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        setup(&mut app);
        animation::setup(&mut app);
        app.configure_sets(
            PostUpdate,
            VrmSystems::LookAt.after(bevy::app::AnimationSystems),
        );

        let mut roots = Vec::new();
        for (i, target) in [
            Vec3::new(1., 2., 1.),
            Vec3::new(-1., 2., 1.),
            Vec3::new(0., 3., 0.),
            Vec3::new(0., 2., 0.),
        ]
        .into_iter()
        .enumerate()
        {
            let root = app.world_mut().spawn(Transform::default()).id();
            let hips = app
                .world_mut()
                .spawn((Transform::from_xyz(0., 1., 0.), ChildOf(root)))
                .id();
            let head = app
                .world_mut()
                .spawn((Transform::from_xyz(0., 1., 0.), ChildOf(hips)))
                .id();
            let eye = app
                .world_mut()
                .spawn((Transform::default(), ChildOf(head)))
                .id();
            let presets = if i == 2 {
                serde_json::json!({"lookUp":{}})
            } else {
                serde_json::json!({"lookLeft":{},"lookRight":{},"lookUp":{}})
            };
            let model: schema::Model = serde_json::from_value(serde_json::json!({
                "specVersion":"1.0",
                "meta":{"name":"Test","authors":["Test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"},
                "humanoid":{"humanBones":{"hips":{"node":0},"head":{"node":1},"leftEye":{"node":2}}},
                "expressions":{"preset":presets},
                "lookAt":{"offsetFromHeadBone":[0.,0.,0.],"type":"expression","rangeMapHorizontalOuter":{"inputMaxValue":90.,"outputScale":1.},"rangeMapVerticalUp":{"inputMaxValue":90.,"outputScale":1.}}
            })).unwrap();
            let springs = (i == 0).then(|| serde_json::from_value::<schema::Springs>(serde_json::json!({
                "specVersion":"1.0",
                "colliders":[{"node":0,"shape":{"sphere":{"offset":[10.,20.,30.],"radius":0.1}}},{"node":0,"shape":{"capsule":{"offset":[10.,20.,30.],"tail":[11.,22.,33.],"radius":0.2}}}],
                "colliderGroups":[{"colliders":[0,1]}],
                "springs":[{"joints":[{"node":0},{"node":1}],"colliderGroups":[0]}]
            })).unwrap());
            let nodes = HashMap::from_iter([(0, hips), (1, head), (2, eye)]);
            app.world_mut()
                .run_system_once(bevy::transform::systems::propagate_transforms_for::<()>)
                .unwrap();
            attach(app.world_mut(), root, &model, springs.as_ref(), &[], &nodes).unwrap();
            app.world_mut().get_mut::<LookAt>(root).unwrap().target = Some(target);
            roots.push(root);
        }
        assert!(
            app.world()
                .get::<SpringJointProps>(
                    app.world()
                        .get::<Vrm>(roots[0])
                        .unwrap()
                        .bone("hips")
                        .unwrap()
                )
                .is_some()
        );
        assert!(
            app.world()
                .get::<SpringJointProps>(
                    app.world()
                        .get::<Vrm>(roots[0])
                        .unwrap()
                        .bone("head")
                        .unwrap()
                )
                .is_none()
        );

        app.update();
        let val = |app: &App, root, name| {
            let ch = app
                .world()
                .get::<crate::expressions::Binds>(root)
                .unwrap()
                .target(name)
                .unwrap();
            app.world().get::<Transform>(ch).unwrap().translation.x
        };
        assert!(
            (val(&app, roots[0], "lookLeft") - 0.5).abs() < 1e-5
                && val(&app, roots[0], "lookRight") == 0.
        );
        assert!((val(&app, roots[1], "lookRight") - 0.5).abs() < 1e-5);
        assert_eq!(val(&app, roots[2], "lookUp"), 1.);
        for name in ["lookLeft", "lookRight", "lookUp"] {
            assert_eq!(val(&app, roots[3], name), 0.);
        }
    }

    #[test]
    fn bone_look_at_without_eyes_falls_back_to_expressions() {
        use bevy::ecs::system::RunSystemOnce;
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, TransformPlugin));
        setup(&mut app);
        animation::setup(&mut app);
        app.configure_sets(
            PostUpdate,
            VrmSystems::LookAt.after(bevy::app::AnimationSystems),
        );

        let root = app.world_mut().spawn(Transform::default()).id();
        let hips = app
            .world_mut()
            .spawn((Transform::from_xyz(0., 1., 0.), ChildOf(root)))
            .id();
        let head = app
            .world_mut()
            .spawn((Transform::from_xyz(0., 1., 0.), ChildOf(hips)))
            .id();
        let model: schema::Model = serde_json::from_value(serde_json::json!({
            "specVersion":"1.0",
            "meta":{"name":"Test","authors":["Test"],"licenseUrl":"https://vrm.dev/licenses/1.0/"},
            "humanoid":{"humanBones":{"hips":{"node":0},"head":{"node":1}}},
            "expressions":{"preset":{"lookLeft":{},"lookRight":{},"lookUp":{},"lookDown":{}}},
            "lookAt":{"offsetFromHeadBone":[0.,0.,0.],"type":"bone"}
        }))
        .unwrap();
        let nodes = HashMap::from_iter([(0, hips), (1, head)]);
        app.world_mut()
            .run_system_once(bevy::transform::systems::propagate_transforms_for::<()>)
            .unwrap();
        attach(app.world_mut(), root, &model, None, &[], &nodes).unwrap();
        app.world_mut().get_mut::<LookAt>(root).unwrap().target = Some(Vec3::new(1., 2., 1.));
        app.update();

        let binds = app.world().get::<crate::expressions::Binds>(root).unwrap();
        let look_left = binds.target("lookLeft").unwrap();
        assert!(
            app.world()
                .get::<Transform>(look_left)
                .unwrap()
                .translation
                .x
                > 0.,
            "bone LookAt without eye bones did not fall back to look expressions"
        );
    }
}
