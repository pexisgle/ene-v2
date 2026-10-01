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
    let json = crate::model::read_glb_json(path)?;
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
/// Convert it to model axes before locking horizontal motion, then back to the
/// target parent. This also handles exporter parents with rotation/unit scale.
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

pub fn keep_stationary(
    models: Query<&HipsBoneEntity, With<Vrm>>,
    mut bones: Query<(&RestTransform, &mut Transform, Option<&MotionAxes>), Changed<Transform>>,
) {
    for hips in &models {
        if let Ok((rest, mut pose, axes)) = bones.get_mut(**hips) {
            let delta = pose.translation - rest.translation;
            let model_delta = axes.map_or(delta, |a| a.source_parent_to_model * delta);
            let stationary = Vec3::new(0., model_delta.y, 0.);
            pose.translation = rest.translation
                + axes.map_or(stationary, |a| a.model_to_target_parent * stationary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parent_axes_and_units_preserve_model_vertical_without_reapplying_a_paused_pose() {
        let mut app = App::new();
        app.add_systems(Update, keep_stationary);
        let source_parent = Mat3::from_quat(Quat::from_rotation_x(0.6)) * 2.;
        let target_parent = Mat3::from_quat(Quat::from_rotation_z(-0.3)) * 1.5;
        let rest = Transform::from_xyz(0.12, 1.2, -0.05);
        let raw_delta = source_parent.inverse() * Vec3::new(1., 0.4, -2.);
        let hips = app
            .world_mut()
            .spawn((
                RestTransform(rest),
                Transform::from_translation(rest.translation + raw_delta),
                MotionAxes {
                    source_parent_to_model: source_parent,
                    model_to_target_parent: target_parent.inverse(),
                },
            ))
            .id();
        app.world_mut().spawn((Vrm, HipsBoneEntity(hips)));
        app.update();
        let corrected = app.world().get::<Transform>(hips).unwrap().translation;
        let model_delta = target_parent * (corrected - rest.translation);
        assert!((model_delta - Vec3::new(0., 0.4, 0.)).length() < 1e-5);
        for _ in 0..20 {
            app.update();
        }
        assert_eq!(
            app.world().get::<Transform>(hips).unwrap().translation,
            corrected
        );
    }
    #[test]
    fn anchor_uses_target_local_rest_without_erasing_bob_or_turn() {
        let mut app = App::new();
        app.add_systems(Update, keep_stationary);
        let rest = Transform::from_xyz(0.15, 1.2, -0.3);
        let rotation = Quat::from_rotation_y(0.7);
        let hips = app
            .world_mut()
            .spawn((
                RestTransform(rest),
                Transform::from_xyz(8., 1.4, -9.).with_rotation(rotation),
            ))
            .id();
        app.world_mut().spawn((Vrm, HipsBoneEntity(hips)));
        let source = app
            .world_mut()
            .spawn((RestTransform(rest), Transform::from_xyz(7., 1.8, 6.)))
            .id();
        app.world_mut().spawn((Vrma, HipsBoneEntity(source)));
        app.update();
        let pose = app.world().get::<Transform>(hips).unwrap();
        assert_eq!(pose.translation, Vec3::new(0.15, 1.4, -0.3));
        assert_eq!(pose.rotation, rotation);
        let position = pose.translation;
        assert_eq!(
            app.world().get::<Transform>(source).unwrap().translation.x,
            7.
        );
        app.update();
        assert_eq!(
            app.world().get::<Transform>(hips).unwrap().translation,
            position
        );
    }
}

#[cfg(test)]
mod validation_tests {
    #[test]
    fn rejects_external_or_invalid_vrma_without_starting_an_asset_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("動き.vrma");
        for (json, valid) in [
            (
                serde_json::json!({"asset":{"version":"2.0"},"nodes":[{}],"animations":[{"samplers":[],"channels":[]}],"extensions":{"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":0}}}}}}),
                true,
            ),
            (
                serde_json::json!({"asset":{"version":"2.0"},"nodes":[{}],"animations":[{"samplers":[],"channels":[]}],"extensions":{"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":0}}}}},"buffers":[{"uri":"../secret.bin"}]}),
                false,
            ),
            (
                serde_json::json!({"asset":{"version":"2.0"},"nodes":[{}],"animations":[{"samplers":[],"channels":[]}],"extensions":{"VRMC_vrm_animation":{"specVersion":"1.0","humanoid":{"humanBones":{"hips":{"node":2}}}}}}),
                false,
            ),
        ] {
            let mut json = serde_json::to_vec(&json).unwrap();
            while json.len() % 4 != 0 {
                json.push(b' ');
            }
            let mut glb = b"glTF".to_vec();
            glb.extend(2u32.to_le_bytes());
            glb.extend((20 + json.len() as u32).to_le_bytes());
            glb.extend((json.len() as u32).to_le_bytes());
            glb.extend(b"JSON");
            glb.extend(json);
            std::fs::write(&path, glb).unwrap();
            assert_eq!(super::validate(&path).is_ok(), valid);
        }
        std::fs::remove_file(path).unwrap();
    }
}
