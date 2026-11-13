pub(crate) mod constraint;
pub(crate) mod look_at;
pub(crate) mod spring;

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
use vrm_spec::vrmc_vrm_1_0::Meta;

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
    pub meta: Meta,
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
                frame: animation::RestFrame {
                    local: local.rotation,
                    global: global.rotation().normalize(),
                },
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
        world.entity_mut(root).insert(look_at::Gaze {
            definition: definition.clone(),
            input,
            active: false,
        });
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
        spring::attach(world, root, springs, nodes)?;
    }
    Ok(())
}
