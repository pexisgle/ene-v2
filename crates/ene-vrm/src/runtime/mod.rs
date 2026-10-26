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
    animation::AnimatedBy,
    ecs::entity::{EntityMapper, MapEntities},
    math::Affine3A,
    platform::collections::HashMap,
    prelude::*,
};

/// Metadata and humanoid bones of one VRM instance. Entity references remap
/// per scene instance.
///
/// `vrm-spec` types do not implement `Reflect`, so the component is opaque.
/// Scene instancing clones the whole component and then remaps [`Self::bones`]
/// through [`MapEntities`].
#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone, opaque)]
#[component(map_entities)]
pub struct Vrm {
    pub bones: HashMap<schema::HumanBoneName, Entity>,
    pub meta: schema::VrmMeta,
}

impl Vrm {
    pub fn bone(&self, name: schema::HumanBoneName) -> Option<Entity> {
        self.bones.get(&name).copied()
    }
}

impl MapEntities for Vrm {
    fn map_entities<M: EntityMapper>(&mut self, mapper: &mut M) {
        for entity in self.bones.values_mut() {
            *entity = mapper.get_mapped(*entity);
        }
    }
}

impl Default for Vrm {
    fn default() -> Self {
        Self {
            bones: HashMap::default(),
            meta: schema::VrmMeta {
                allow_antisocial_or_hate_usage: None,
                allow_excessively_sexual_usage: None,
                allow_excessively_violent_usage: None,
                allow_political_or_religious_usage: None,
                allow_redistribution: None,
                authors: Vec::new(),
                avatar_permission: None,
                commercial_usage: None,
                contact_information: None,
                copyright_information: None,
                credit_notation: None,
                extensions: None,
                extras: None,
                license_url: String::new(),
                modification: None,
                name: String::new(),
                other_license_url: None,
                references: None,
                third_party_licenses: None,
                thumbnail_image: None,
                version: None,
            },
        }
    }
}

pub(crate) fn setup(app: &mut App) {
    app.register_type::<Vrm>();
    look_at::setup(app);
    constraint::setup(app);
    spring::setup(app);
}

/// Height of the hips above the scene root, used to normalize VRMA translation.
pub(crate) fn hips_height(
    world: &World,
    nodes: &HashMap<usize, Entity>,
    root_inv: Affine3A,
    node: Option<usize>,
) -> f32 {
    node.and_then(|node| world.get::<GlobalTransform>(*nodes.get(&node)?))
        .map(|global| root_inv.transform_point3(global.translation()).y.abs())
        .filter(|&height| height >= f32::EPSILON)
        .unwrap_or(1.)
}

pub(crate) fn attach(
    world: &mut World,
    root: Entity,
    model: &schema::Model,
    springs: Option<&schema::Springs>,
    constraints: &[(usize, schema::NodeConstraint)],
    nodes: &HashMap<usize, Entity>,
) -> std::io::Result<()> {
    let constraints = constraint::compile_constraints(constraints, world, nodes)?;
    let root_inv = world
        .get::<GlobalTransform>(root)
        .expect("native scene transform")
        .affine()
        .inverse();
    let hips = model
        .humanoid
        .human_bones
        .0
        .get(&schema::HumanBoneName::Hips)
        .and_then(|bone| bone.as_ref())
        .and_then(|bone| bone.node)
        .map(schema::index);
    let height = hips_height(world, nodes, root_inv, hips);
    let mut bones = HashMap::default();
    for (name, bone) in &model.humanoid.human_bones.0 {
        let bone = bone
            .as_ref()
            .ok_or_else(|| invalid("VRMC human bone is missing"))?;
        let node = bone
            .node
            .ok_or_else(|| invalid("VRMC human bone requires a node"))?;
        let entity = require_node(nodes, schema::index(node))?;
        let (&local, global) = world
            .entity(entity)
            .components::<(&Transform, &GlobalTransform)>();
        let global = GlobalTransform::from(root_inv * global.affine());
        world.entity_mut(entity).insert((
            RestPose {
                local,
                global_rotation: global.rotation().normalize(),
                translation_from_normalized: (local.compute_affine() * global.affine().inverse())
                    .matrix3
                    * height,
            },
            Pose::default(),
            animation::bone_target(*name),
            AnimatedBy(root),
        ));
        bones.insert(*name, entity);
    }
    if let Some(definition) = &model.look_at {
        if let Some(offset) = &definition.offset_from_head_bone
            && offset.len() != 3
        {
            return Err(invalid("VRMC lookAt offsetFromHeadBone requires 3 values"));
        }
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
    expressions::attach(world, root, model.expressions.as_ref(), nodes)?;
    if let Some(springs) = springs {
        let colliders = springs
            .colliders
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|collider| {
                let entity = require_node(nodes, schema::index(collider.node))?;
                let (points, radius) = match (&collider.shape.sphere, &collider.shape.capsule) {
                    (Some(sphere), None) => (
                        [spring_point(sphere.offset); 2],
                        spring_scalar(sphere.radius),
                    ),
                    (None, Some(capsule)) => (
                        [spring_point(capsule.offset), spring_point(capsule.tail)],
                        spring_scalar(capsule.radius),
                    ),
                    _ => {
                        return Err(invalid(
                            "VRMC collider requires exactly one of sphere or capsule",
                        ));
                    }
                };
                if !radius.is_finite()
                    || radius < 0.
                    || !points[0].is_finite()
                    || !points[1].is_finite()
                {
                    return Err(invalid("VRMC collider shape is invalid"));
                }
                Ok(spring::Collider {
                    entity,
                    segment: Segment3d::from(points),
                    radius,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let groups = springs.collider_groups.as_deref().unwrap_or_default();
        if groups
            .iter()
            .any(|group| group.colliders.iter().any(|&i| i >= colliders.len()))
        {
            return Err(invalid("VRMC collider out of range"));
        }
        let mut chains = Vec::new();
        for spring_def in springs.springs.as_deref().unwrap_or_default() {
            if spring_def.joints.is_empty() {
                return Err(invalid("VRMC spring requires joints"));
            }
            let mut joints = Vec::with_capacity(spring_def.joints.len());
            for joint in &spring_def.joints {
                let entity = require_node(nodes, schema::index(joint.node))?;
                let props = spring_props(joint);
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
                joints.push((entity, props));
            }
            let mut chain_joints = Vec::new();
            for pair in joints.windows(2) {
                let (entity, props) = pair[0];
                let next = pair[1].0;
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
                world.entity_mut(entity).insert(props);
                chain_joints.push(spring::SpringJoint {
                    entity,
                    rest: local,
                    tail,
                    state: None,
                    rotation: None,
                });
            }
            let mut chain_colliders = Vec::new();
            for &group in spring_def.collider_groups.as_deref().unwrap_or_default() {
                let group = usize::try_from(group)
                    .map_err(|_| invalid("VRMC collider group out of range"))?;
                chain_colliders.extend_from_slice(
                    &groups
                        .get(group)
                        .ok_or_else(|| invalid("VRMC collider group out of range"))?
                        .colliders,
                );
            }
            chains.push(spring::SpringChain {
                joints: chain_joints,
                colliders: chain_colliders,
                center: spring_def
                    .center
                    .map(|center| require_node(nodes, schema::index(center)))
                    .transpose()?,
            });
        }
        // Chains cannot share joints, but a chain may start below another
        // chain's tail. Evaluate ancestors first so the child chain reads this
        // frame's parent pose instead of being one frame late.
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
        world.entity_mut(root).insert(spring::SpringRig {
            chains,
            colliders,
            paused: false,
        });
    }
    Ok(())
}

fn spring_point(value: Option<[f64; 3]>) -> Vec3 {
    value.map_or(Vec3::ZERO, |[x, y, z]| {
        Vec3::new(x as f32, y as f32, z as f32)
    })
}

fn spring_scalar(value: Option<f64>) -> f32 {
    value.unwrap_or(0.) as f32
}

fn spring_props(joint: &vrm_spec::vrmc_spring_bone_1_0::SpringBoneJoint) -> SpringJointProps {
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
