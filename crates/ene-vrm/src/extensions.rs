//! Reads the VRMC extensions of a glTF document into scene state: the VRM or
//! VRMA definition, MToon materials, spring bones and node constraints.

use crate::{
    loader::{MotionScene, VrmScene, invalid},
    material::{self, MToonExtension, MToonMaterial},
    runtime, schema,
};
use bevy::{
    asset::LoadContext,
    ecs::system::RunSystemOnce,
    gltf::{
        GltfAssetLabel, GltfLoaderSettings, GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler},
    },
    platform::collections::HashMap,
    prelude::*,
};
use serde::Deserialize;
use std::sync::Arc;
use vrm_spec::{
    vrmc_materials_mtoon_1_0::VRMC_MATERIALS_MTOON, vrmc_spring_bone_1_0::VRMC_SPRING_BONE,
    vrmc_vrm_1_0::VRMC_VRM, vrmc_vrm_animation_1_0::VRMC_VRM_ANIMATION,
};

pub(crate) const VRMC_NODE_CONSTRAINT: &str = "VRMC_node_constraint";

/// Every VRM 1.0 extension this loader reads requires the same version.
fn require_v1(version: &str, extension: &str) -> std::io::Result<()> {
    if version == "1.0" {
        Ok(())
    } else {
        Err(invalid(format!("{extension} requires specVersion 1.0")))
    }
}
pub(crate) fn extension<T: serde::de::DeserializeOwned>(
    value: Option<&serde_json::Value>,
    name: &str,
) -> std::io::Result<T> {
    T::deserialize(value.ok_or_else(|| invalid(format!("missing {name}")))?).map_err(invalid)
}

/// The JSON schema requires a node for every animated human bone, expression
/// and look-at target, but `vrm-spec` models them as optional.
fn validate_motion(motion: &schema::Motion) -> std::io::Result<()> {
    if let Some(humanoid) = &motion.humanoid {
        for bone in humanoid.human_bones.0.values() {
            if bone.as_ref().and_then(|bone| bone.node).is_none() {
                return Err(invalid("VRMA human bone requires a node"));
            }
        }
    }
    if let Some(expressions) = &motion.expressions {
        if let Some(preset) = &expressions.preset {
            for expression in preset.0.values() {
                if expression.node.is_none() {
                    return Err(invalid("VRMA expression requires a node"));
                }
            }
        }
        if let Some(custom) = &expressions.custom {
            for expression in custom.values() {
                if expression.node.is_none() {
                    return Err(invalid("VRMA expression requires a node"));
                }
            }
        }
    }
    if motion
        .look_at
        .as_ref()
        .is_some_and(|look_at| look_at.node.is_none())
    {
        return Err(invalid("VRMA lookAt requires a node"));
    }
    Ok(())
}

/// Returns an image handle that samples as `space`, cloning the decoded image
/// when Bevy loaded the shared texture for the other purpose.
///
/// Bevy picks the format from the core glTF slots (base color is sRGB,
/// normal/occlusion/metallic-roughness are linear). An MToon slot can share the
/// same texture index for the opposite purpose, so neither the original format
/// nor a one-way conversion is sufficient. The sampled bytes are unchanged;
/// only the format suffix controls the GPU transfer function.
fn texture_in_space(
    context: &mut LoadContext<'_>,
    index: usize,
    original: &Handle<Image>,
    space: material::TextureSpace,
) -> Option<Handle<Image>> {
    let label = GltfAssetLabel::Texture(index).to_string();
    // ponytail: only GLB-embedded images are converted; external URI textures
    // would need their own asset load with the opposite `is_srgb` setting.
    let image = context.get_labeled(&label)?.get::<Image>()?;
    let format = image.texture_descriptor.format;
    let wants_srgb = space == material::TextureSpace::Srgb;
    if format.is_srgb() == wants_srgb {
        return Some(original.clone());
    }
    let converted = if wants_srgb {
        format.add_srgb_suffix()
    } else {
        format.remove_srgb_suffix()
    };
    if converted == format {
        // The format has no variant for the requested transfer function
        // (floating point, two-channel compressed, ...). Keep the decoded data.
        return Some(original.clone());
    }
    let mut image = image.clone();
    image.texture_descriptor.format = converted;
    let suffix = if wants_srgb { "srgb" } else { "linear" };
    Some(context.add_labeled_asset(format!("{label}/{suffix}"), image))
}

enum Definition {
    Model(Box<schema::Model>),
    Motion(Arc<schema::Motion>),
}

#[derive(Default)]
pub(crate) struct VrmExtensions {
    definition: Option<std::io::Result<Definition>>,
    /// glTF scene that Bevy's `default_scene` (or the first scene) resolves to.
    selected_scene: usize,
    materials: HashMap<usize, schema::Toon>,
    springs: Option<schema::Springs>,
    nodes: HashMap<usize, Entity>,
    textures: Vec<Handle<Image>>,
    converted: HashMap<(usize, material::TextureSpace), Handle<Image>>,
    constraints: Vec<(usize, schema::NodeConstraint)>,
}

impl VrmExtensions {
    fn ensure_texture(
        &mut self,
        context: &mut LoadContext<'_>,
        index: usize,
        space: material::TextureSpace,
    ) {
        if self.converted.contains_key(&(index, space)) {
            return;
        }
        let Some(original) = self.textures.get(index).cloned() else {
            return;
        };
        if let Some(texture) = texture_in_space(context, index, &original, space) {
            self.converted.insert((index, space), texture);
        }
    }
}

// Reject data this crate cannot use (missing extension, dangling indices,
// constraint cycles), but leave VRM schema conformance to authoring tools so a
// renderable file still loads.
// ponytail: no JSON Schema pass; add one if import diagnostics are needed.
impl GltfExtensionHandler for VrmExtensions {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::<Self>::default()
    }
    fn on_root(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Gltf,
        _settings: &GltfLoaderSettings,
    ) {
        let suffix = context.path().path().extension().and_then(|x| x.to_str());
        let motion = suffix == Some("vrma");
        if !motion && suffix != Some("vrm") {
            return;
        }
        self.selected_scene = source.default_scene().map_or(0, |scene| scene.index());
        self.definition = Some((|| {
            if motion {
                let motion: schema::Motion = extension(
                    source.extension_value(VRMC_VRM_ANIMATION),
                    "VRMC_vrm_animation",
                )?;
                require_v1(&motion.spec_version, "VRMC_vrm_animation")?;
                validate_motion(&motion)?;
                return Ok(Definition::Motion(Arc::new(motion)));
            }
            let model: schema::Model = extension(source.extension_value(VRMC_VRM), "VRMC_vrm")?;
            require_v1(&model.spec_version, "VRMC_vrm")?;
            for material in source.materials() {
                let Some(value) = material.extension_value(VRMC_MATERIALS_MTOON) else {
                    continue;
                };
                if let Some(shift) = value
                    .get("shadingShiftTexture")
                    .filter(|shift| !shift.is_null())
                {
                    gltf::json::texture::Info::deserialize(shift).map_err(invalid)?;
                }
                let authored =
                    vrm_spec::vrmc_materials_mtoon_1_0::VrmcMaterialsMtoonSchema::deserialize(
                        value,
                    )
                    .map_err(invalid)?;
                require_v1(&authored.spec_version, "VRMC_materials_mtoon")?;
                let toon = schema::Toon::compile(authored).map_err(invalid)?;
                if let Some(index) = material.index() {
                    self.materials.insert(index, toon);
                }
            }
            self.springs = source
                .extension_value(VRMC_SPRING_BONE)
                .map(schema::Springs::deserialize)
                .transpose()
                .map_err(invalid)?;
            if let Some(springs) = &self.springs {
                require_v1(&springs.spec_version, "VRMC_springBone")?;
            }
            for node in source.nodes() {
                let Some(value) = node.extension_value(VRMC_NODE_CONSTRAINT) else {
                    continue;
                };
                let constraint: schema::NodeConstraint =
                    extension(Some(value), VRMC_NODE_CONSTRAINT)?;
                require_v1(&constraint.spec_version, "VRMC_node_constraint")?;
                self.constraints.push((node.index(), constraint));
            }
            Ok(Definition::Model(Box::new(model)))
        })());
    }
    fn on_texture(&mut self, texture: &gltf::Texture, image: Handle<Image>) {
        if matches!(self.definition, Some(Ok(Definition::Model(_)))) {
            if self.textures.len() <= texture.index() {
                self.textures.resize(texture.index() + 1, Handle::default());
            }
            self.textures[texture.index()] = image;
        }
    }
    fn on_material(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Material,
        _: Handle<GltfMaterial>,
        _: &GltfMaterial,
        label: &str,
    ) {
        if !matches!(self.definition, Some(Ok(Definition::Model(_)))) {
            return;
        }
        let Some(toon) = source.index().and_then(|i| self.materials.get(&i)).cloned() else {
            return;
        };
        let Some(mut base) = context
            .get_labeled(format!("{label}/std"))
            .and_then(|a| a.get::<StandardMaterial>())
            .cloned()
        else {
            return;
        };
        base.unlit = false;
        base.opaque_render_method = bevy::material::OpaqueRendererMethod::Forward;
        for slot in material::NUMERIC_SLOTS
            .into_iter()
            .chain(material::COLOR_SLOTS)
        {
            let Some(info) = toon.textures()[slot] else {
                continue;
            };
            self.ensure_texture(context, info.index.value(), material::texture_space(slot));
        }
        // The core color slots also use the shared texture index. Bevy may have
        // loaded it linearly because another material uses it as a numeric
        // texture; the MToon material still needs the sRGB view.
        let base_color = source
            .pbr_metallic_roughness()
            .base_color_texture()
            .map(|info| info.texture().index());
        let emissive = source.emissive_texture().map(|info| info.texture().index());
        for index in [base_color, emissive].into_iter().flatten() {
            self.ensure_texture(context, index, material::TextureSpace::Srgb);
        }
        if let Some(index) = base_color {
            base.base_color_texture = self
                .converted
                .get(&(index, material::TextureSpace::Srgb))
                .cloned()
                .or(base.base_color_texture.clone());
        }
        if let Some(index) = emissive {
            base.emissive_texture = self
                .converted
                .get(&(index, material::TextureSpace::Srgb))
                .cloned()
                .or(base.emissive_texture.clone());
        }
        let material = MToonMaterial {
            base,
            extension: MToonExtension::new(&toon, &self.textures, &self.converted),
        };
        let label = format!("{label}/vrm");
        let outline_mode = toon.outline_mode();
        if outline_mode != schema::OutlineMode::None {
            let mut outline = material.clone();
            outline.base.double_sided = false;
            if outline.base.cull_mode.is_none() && label.ends_with("(inverted)/vrm") {
                outline.base.cull_mode = Some(bevy::render::render_resource::Face::Front);
            }
            outline.extension.outline = Some(outline_mode);
            context.add_labeled_asset(format!("{label}/outline"), outline);
        }
        context.add_labeled_asset(label, material);
    }
    fn on_spawn_mesh_and_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _primitive: &gltf::Primitive,
        _: &gltf::Mesh,
        source: &gltf::Material,
        entity: &mut EntityWorldMut,
        label: &str,
    ) {
        if !matches!(self.definition, Some(Ok(Definition::Model(_)))) {
            return;
        }
        if let Some(index) = source.index() {
            let base_uv = source
                .pbr_metallic_roughness()
                .base_color_texture()
                .and_then(|info| {
                    info.texture_transform()
                        .map(|t| schema::UvTransform::from_gltf(&t))
                })
                .unwrap_or_default();
            entity.insert((
                material::MaterialSlot(index),
                material::BaseUv(base_uv),
                material::EmissiveStrength(source.emissive_strength().unwrap_or(1.)),
            ));
            if self.materials.contains_key(&index)
                && entity.contains::<MeshMaterial3d<StandardMaterial>>()
            {
                entity
                    .remove::<MeshMaterial3d<StandardMaterial>>()
                    .insert(MeshMaterial3d::<MToonMaterial>(
                        context.get_label_handle(format!("{label}/vrm")),
                    ));
            }
        }
    }
    fn on_gltf_node(
        &mut self,
        _: &mut LoadContext<'_>,
        node: &gltf::Node,
        entity: &mut EntityWorldMut,
    ) {
        if self.definition.is_some() {
            entity.insert(crate::animation::NativeNode(node.index()));
            self.nodes.insert(node.index(), entity.id());
        }
    }
    fn on_scene_completed(
        &mut self,
        context: &mut LoadContext<'_>,
        scene: &gltf::Scene,
        root: Entity,
        world: &mut World,
    ) {
        let Some(definition) = &self.definition else {
            return;
        };
        world
            .entity_mut(root)
            .insert(crate::animation::NativeSceneRoot);
        // VRM metadata is document-wide but nodes are per-scene. Only the scene
        // that Bevy (and `VrmLoader::load`) uses may be attached.
        if scene.index() != self.selected_scene {
            self.nodes.clear();
            return;
        }
        let result = (|| {
            let definition = definition.as_ref().map_err(|e| invalid(e.to_string()))?;
            world
                .run_system_once(bevy::transform::systems::propagate_transforms_for::<()>)
                .map_err(|e| invalid(format!("{e:?}")))?;
            match definition {
                Definition::Model(model) => {
                    material::add_outlines(world, &mut |slot, handle| {
                        self.materials
                            .get(&slot)
                            .filter(|toon| toon.outline_mode() != schema::OutlineMode::None)?;
                        Some(
                            context
                                .get_label_handle(format!("{}/outline", handle.path()?.label()?)),
                        )
                    });
                    let constraints: Vec<_> = self
                        .constraints
                        .iter()
                        .filter(|(node, _)| self.nodes.contains_key(node))
                        .cloned()
                        .collect();
                    runtime::attach(
                        world,
                        root,
                        model,
                        self.springs.as_ref(),
                        &constraints,
                        &self.nodes,
                    )?;
                    Ok(None)
                }
                Definition::Motion(motion) => Ok(Some(MotionScene {
                    source: motion.clone(),
                    nodes: std::mem::take(&mut self.nodes),
                })),
            }
        })();
        world.entity_mut(root).insert(VrmScene(result));
        self.nodes.clear();
    }
}
