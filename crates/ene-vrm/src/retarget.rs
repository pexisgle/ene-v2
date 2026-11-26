//! Translates a VRMA glTF motion onto this crate's typed animation targets.
//! The pose components and their systems live in [`crate::animation`].

use crate::{
    animation::{
        ExpressionInput, GazeInput, NativeNode, NativeSceneRoot, Pose, RestFrame, bone_target,
        expression_target, gaze_target,
    },
    expressions::ExpressionName,
    loader::{invalid, require_node},
    schema,
};
use bevy::{
    animation::{
        AnimationTargetId, animated_field,
        animation_curves::{AnimatableCurve, AnimatableProperty, AnimatedField, EvaluatorId},
    },
    curve::{FunctionCurve, Interval},
    platform::collections::{HashMap, HashSet},
    prelude::*,
};
use std::sync::Arc;

fn collect_native_targets(
    world: &World,
    entity: Entity,
    visited: &mut HashSet<usize>,
    targets: &mut HashMap<usize, AnimationTargetId>,
) {
    let Some(node) = world.get::<NativeNode>(entity) else {
        return;
    };
    if !visited.insert(node.0) {
        return;
    }
    if let Some(children) = world.get::<Children>(entity) {
        for child in children.iter() {
            if world.get::<NativeNode>(child).is_some() {
                collect_native_targets(world, child, visited, targets);
            }
        }
    }
    let id = if let Some(id) = world.get::<AnimationTargetId>(entity) {
        *id
    } else {
        // Skipped native channels still participate in collision checks.
        // Read Bevy's actual names, excluding its synthetic scene root.
        let mut path = Vec::new();
        let mut current = Some(entity);
        while let Some(id) = current {
            let entity = world.entity(id);
            if !entity.contains::<NativeNode>() {
                break;
            }
            if let Some(name) = entity.get::<Name>() {
                path.push(name.clone());
            }
            current = entity.get::<ChildOf>().map(ChildOf::parent);
        }
        AnimationTargetId::from_names(path.iter().rev())
    };
    targets.insert(node.0, id);
}

fn motion_targets(
    document: &gltf::Document,
    context: &bevy::asset::LoadContext<'_>,
) -> std::io::Result<HashMap<usize, AnimationTargetId>> {
    let mut targets = HashMap::default();
    for scene in document.scenes() {
        let label = format!("Scene{}", scene.index());
        let world = &context
            .get_labeled(&label)
            .and_then(|asset| asset.get::<WorldAsset>())
            .ok_or_else(|| invalid("VRMA native scene unavailable"))?
            .world;
        let Some(root) = world
            .iter_entities()
            .find(bevy::ecs::world::EntityRef::contains::<NativeSceneRoot>)
        else {
            return Err(invalid("VRMA native scene root unavailable"));
        };
        if let Some(children) = root.get::<Children>() {
            // Native curves use first-visited descendants per source root and
            // last-root/last-scene overwrite. ECS iteration order cannot decide it.
            for entity in children.iter() {
                if world.get::<NativeNode>(entity).is_some() {
                    collect_native_targets(world, entity, &mut HashSet::default(), &mut targets);
                }
            }
        }
    }
    Ok(targets)
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

fn resolve_hips_height(
    motion: &schema::Motion,
    world: &World,
    nodes: &HashMap<usize, Entity>,
    root_inv: bevy::math::Affine3A,
) -> f32 {
    let hips = motion
        .humanoid
        .iter()
        .flat_map(|humanoid| humanoid.human_bones.0.get(&schema::HumanBoneName::Hips))
        .next()
        .and_then(|bone| bone.as_ref())
        .and_then(|bone| bone.node)
        .map(schema::index);
    crate::runtime::hips_height(world, nodes, root_inv, hips)
}

struct RetargetContext<'a> {
    world: &'a World,
    nodes: &'a HashMap<usize, Entity>,
    targets: &'a HashMap<usize, AnimationTargetId>,
    bones: &'a HashMap<usize, schema::HumanBoneName>,
    expressions: &'a HashMap<usize, ExpressionName>,
    gaze_node: Option<usize>,
    root_inv: bevy::math::Affine3A,
    height: f32,
    rot_id: EvaluatorId<'a>,
    trans_id: EvaluatorId<'a>,
}

fn retarget_channel(
    ctx: &RetargetContext<'_>,
    animation: &gltf::Animation<'_>,
    native: &AnimationClip,
    channel: &gltf::animation::Channel<'_>,
    clip: &mut AnimationClip,
) -> std::io::Result<()> {
    let node = channel.target().node().index();
    let property = channel.target().property();
    let bone = ctx.bones.get(&node).copied();
    let expression = ctx.expressions.get(&node);
    if !is_relevant_channel(
        property,
        bone,
        expression.is_some(),
        ctx.gaze_node == Some(node),
    ) {
        return Ok(());
    }
    let Some((entity, curve)) = validated_channel_curve(ctx, animation, native, node, property)?
    else {
        return Ok(());
    };
    let domain = curve.0.domain();
    let (&local, global) = ctx
        .world
        .entity(entity)
        .components::<(&Transform, &GlobalTransform)>();
    let global = GlobalTransform::from(ctx.root_inv * global.affine());
    let eval = curve.0.evaluator_id();
    if eval == ctx.rot_id
        && let Some(bone) = bone
    {
        let frame = RestFrame {
            local: local.rotation,
            global: global.rotation().normalize(),
        };
        add_bone_rotation_curve(clip, bone, domain, frame, curve);
    } else if eval == ctx.trans_id && bone == Some(schema::HumanBoneName::Hips) {
        let hips = HipsRetarget {
            parent: global.affine() * local.compute_affine().inverse(),
            rest_translation: local.translation,
            height: ctx.height,
        };
        add_hips_translation_curve(clip, domain, hips, curve);
    } else if eval == ctx.trans_id
        && let Some(expression) = expression
    {
        add_expression_curve(clip, expression, domain, curve);
    } else if eval == ctx.rot_id && ctx.gaze_node == Some(node) {
        add_gaze_curve(clip, domain, curve);
    }
    Ok(())
}

fn is_relevant_channel(
    property: gltf::animation::Property,
    bone: Option<schema::HumanBoneName>,
    has_expression: bool,
    is_gaze: bool,
) -> bool {
    match property {
        gltf::animation::Property::Rotation => bone.is_some() || is_gaze,
        gltf::animation::Property::Translation => {
            bone == Some(schema::HumanBoneName::Hips) || has_expression
        }
        _ => false,
    }
}

fn validated_channel_curve<'a>(
    ctx: &RetargetContext<'_>,
    animation: &gltf::Animation<'_>,
    native: &'a AnimationClip,
    node: usize,
    property: gltf::animation::Property,
) -> std::io::Result<Option<(Entity, &'a bevy::animation::VariableCurve)>> {
    let entity = require_node(ctx.nodes, node)?;
    let id = ctx
        .world
        .get::<AnimationTargetId>(entity)
        .ok_or_else(|| invalid("VRMA animated node has no native target"))?;
    let native_id = ctx
        .targets
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
    let Some(curve) = native.curves().get(&native_id).and_then(|curves| {
        curves.iter().find(|curve| match property {
            gltf::animation::Property::Rotation => curve.0.evaluator_id() == ctx.rot_id,
            gltf::animation::Property::Translation => curve.0.evaluator_id() == ctx.trans_id,
            _ => false,
        })
    }) else {
        return Ok(None);
    };
    // ponytail: reject instead of matching curves by channel order;
    // revisit if Bevy exposes per-node curve identity. The scan is
    // quadratic in channel count, which keeps VRMA-sized inputs simple.
    if let Some(other) = animation.channels().find(|other| {
        let target = other.target();
        target.node().index() != node
            && target.property() == property
            && ctx.targets.get(&target.node().index()) == Some(&native_id)
    }) {
        let other = other.target().node().index();
        return Err(invalid(format!(
            "VRMA nodes {node} and {other} share the same animation target path"
        )));
    }
    Ok(Some((entity, curve)))
}

#[derive(Clone)]
struct HipsRetarget {
    parent: bevy::math::Affine3A,
    rest_translation: Vec3,
    height: f32,
}

impl HipsRetarget {
    fn normalize(&self, translation: Vec3) -> Vec3 {
        self.parent
            .transform_vector3(translation - self.rest_translation)
            / self.height
    }
}

fn add_bone_rotation_curve(
    clip: &mut AnimationClip,
    bone: schema::HumanBoneName,
    domain: Interval,
    frame: RestFrame,
    curve: &bevy::animation::VariableCurve,
) {
    let curve = Arc::new(curve.clone());
    clip.add_curve_to_target(
        bone_target(bone),
        AnimatableCurve::new(
            AnimatedField::new_unchecked("rotation", |pose: &mut Pose| {
                pose.rotation.get_or_insert(Quat::IDENTITY)
            }),
            FunctionCurve::new(domain, move |t| {
                frame.normalize(
                    curve
                        .0
                        .sample_clamped(t)
                        .downcast::<Quat>()
                        .map_or(Quat::IDENTITY, |q| *q),
                )
            }),
        ),
    );
}

fn add_hips_translation_curve(
    clip: &mut AnimationClip,
    domain: Interval,
    hips: HipsRetarget,
    curve: &bevy::animation::VariableCurve,
) {
    let curve = Arc::new(curve.clone());
    clip.add_curve_to_target(
        bone_target(schema::HumanBoneName::Hips),
        AnimatableCurve::new(
            AnimatedField::new_unchecked("translation", |pose: &mut Pose| {
                pose.translation.get_or_insert(Vec3::ZERO)
            }),
            FunctionCurve::new(domain, move |t| {
                hips.normalize(
                    curve
                        .0
                        .sample_clamped(t)
                        .downcast::<Vec3>()
                        .map_or(Vec3::ZERO, |v| *v),
                )
            }),
        ),
    );
}

fn add_expression_curve(
    clip: &mut AnimationClip,
    expression: &ExpressionName,
    domain: Interval,
    curve: &bevy::animation::VariableCurve,
) {
    let curve = Arc::new(curve.clone());
    clip.add_curve_to_target(
        expression_target(expression),
        AnimatableCurve::new(
            AnimatedField::new_unchecked("value", |input: &mut ExpressionInput| {
                input.value.get_or_insert(0.)
            }),
            FunctionCurve::new(domain, move |t| {
                curve
                    .0
                    .sample_clamped(t)
                    .downcast::<Vec3>()
                    .map_or(0., |v| v.x)
            }),
        ),
    );
}

fn add_gaze_curve(
    clip: &mut AnimationClip,
    domain: Interval,
    curve: &bevy::animation::VariableCurve,
) {
    let curve = Arc::new(curve.clone());
    clip.add_curve_to_target(
        gaze_target(),
        AnimatableCurve::new(
            AnimatedField::new_unchecked("rotation", |input: &mut GazeInput| {
                input.rotation.get_or_insert(Quat::IDENTITY)
            }),
            FunctionCurve::new(domain, move |t| {
                curve
                    .0
                    .sample_clamped(t)
                    .downcast::<Quat>()
                    .map_or(Quat::IDENTITY, |q| *q)
            }),
        ),
    );
}

/// The declarations the host validates and stores.
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
        .ok_or_else(|| invalid("missing native scene transform"))?
        .affine()
        .inverse();
    let height = resolve_hips_height(motion, world, nodes, root_inv);
    let bones = motion_bones(motion);
    let expressions = motion_expressions(motion);
    let (rot_prop, trans_prop) = (
        animated_field!(Transform::rotation),
        animated_field!(Transform::translation),
    );
    let targets = motion_targets(source, context)?;
    let ctx = RetargetContext {
        world,
        nodes,
        targets: &targets,
        bones: &bones,
        expressions: &expressions,
        gaze_node: motion
            .look_at
            .as_ref()
            .and_then(|look_at| look_at.node)
            .map(schema::index),
        root_inv,
        height,
        rot_id: rot_prop.evaluator_id(),
        trans_id: trans_prop.evaluator_id(),
    };
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
            retarget_channel(&ctx, &animation, native, &channel, &mut clip)?;
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
