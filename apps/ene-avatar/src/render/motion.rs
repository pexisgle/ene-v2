use bevy::prelude::*;
use bevy_vrm1::prelude::*;
use std::path::Path;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnimationExtension {
    spec_version: String,
    humanoid: Humanoid,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Humanoid {
    human_bones: std::collections::BTreeMap<String, bevy_vrm1::prelude::VrmNode>,
}
pub fn validate(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let json = crate::render::model::read_glb_json(path)?;
    let animation: AnimationExtension =
        serde_json::from_value(json["extensions"]["VRMC_vrm_animation"].clone())?;
    if animation.spec_version != "1.0" {
        return Err("VRMA 1.0のモーションを選択してください。".into());
    }
    let nodes = json["nodes"].as_array().ok_or("VRMAの骨がありません。")?;
    let bones = animation.humanoid.human_bones;
    if !bones.contains_key("hips") || bones.values().any(|bone| bone.node >= nodes.len()) {
        return Err("VRMAのhumanoidの骨指定が不正です。".into());
    }
    if json["animations"].as_array().is_none_or(|a| a.len() != 1) {
        return Err("1つのアニメーションを含むVRMAに対応しています。".into());
    }
    if json["meshes"].as_array().is_some_and(|m| !m.is_empty()) {
        return Err("メッシュを含むVRMAは利用できません。".into());
    }
    Ok(())
}

/// Rest-parent bases, with application placement stripped from both rigs.
/// bevy_vrm1's baked hips delta is source-local, scaled by model-space height.
/// Convert the complete delta through model axes into the target parent.
/// Horizontal hips motion is part of the pose: discarding it makes planted
/// feet slide as the animated legs counteract a translation that never happens.
/// Application placement and the camera anchor remain independent of the pose.
#[derive(Component)]
pub struct MotionAxes {
    pub source_parent_to_model: Mat3,
    pub model_to_target_parent: Mat3,
}
impl MotionAxes {
    pub fn from_world(world: &World, model: Entity, motion: Entity) -> Option<(Entity, Self)> {
        fn parent_basis(world: &World, root: Entity, hips: Entity) -> Option<Mat3> {
            let prefix = world.get::<RestWorldTransform>(root)?.affine();
            let global = world.get::<RestGlobalTransform>(hips)?.affine();
            let local = world.get::<RestTransform>(hips)?.compute_affine();
            Some(Mat3::from(
                (prefix.inverse() * global * local.inverse()).matrix3,
            ))
        }
        let target = **world.get::<HipsBoneEntity>(model)?;
        let source = **world.get::<HipsBoneEntity>(motion)?;
        let source_parent_to_model = parent_basis(world, motion, source)?;
        let model_to_target_parent = parent_basis(world, model, target)?.inverse();
        if !source_parent_to_model.is_finite() || !model_to_target_parent.is_finite() {
            return None;
        }
        Some((
            target,
            Self {
                source_parent_to_model,
                model_to_target_parent,
            },
        ))
    }
}

pub fn apply_hips_translation(
    models: Query<&HipsBoneEntity, With<Vrm>>,
    mut bones: Query<(&RestTransform, &mut Transform, &MotionAxes), Changed<Transform>>,
) {
    for hips in &models {
        if let Ok((rest, mut pose, axes)) = bones.get_mut(**hips) {
            let delta = pose.translation - rest.translation;
            let model_delta = axes.source_parent_to_model * delta;
            // Animation already marked this Transform as changed. Avoid marking
            // it again here, which would reapply the basis on a paused pose.
            pose.bypass_change_detection().translation =
                rest.translation + axes.model_to_target_parent * model_delta;
        }
    }
}
