use crate::{
    material::{self, MToonExtension, MToonMaterial},
    runtime, schema,
};
use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    ecs::system::RunSystemOnce,
    gltf::{
        DefaultGltfImageSampler, Gltf, GltfAssetLabel, GltfError, GltfLoader, GltfLoaderSettings,
        GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
    },
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    platform::collections::HashMap,
    prelude::*,
};
use serde::Deserialize;
use std::{borrow::Cow, sync::Arc};
use vrm_spec::{
    vrmc_materials_mtoon_1_0::VRMC_MATERIALS_MTOON, vrmc_spring_bone_1_0::VRMC_SPRING_BONE,
    vrmc_vrm_1_0::VRMC_VRM, vrmc_vrm_animation_1_0::VRMC_VRM_ANIMATION,
};

pub(crate) fn setup(app: &mut App) {
    app.preregister_asset_loader::<VrmLoader>(&["vrm", "vrma"]);
    app.world()
        .get_resource::<GltfExtensionHandlers>()
        .expect("VrmPlugin requires GltfPlugin")
        .0
        .write_blocking()
        .push(Box::<VrmExtensions>::default());
}

pub(crate) fn finish(app: &mut App) {
    let plugin = *app
        .get_added_plugins::<bevy::gltf::GltfPlugin>()
        .first()
        .expect("VrmPlugin requires GltfPlugin");
    let world = app.world();
    app.register_asset_loader(VrmLoader(GltfLoader {
        supported_compressed_formats: world
            .get_resource::<CompressedImageFormatSupport>()
            .map_or(CompressedImageFormats::NONE, |x| x.0),
        custom_vertex_attributes: plugin.custom_vertex_attributes.clone(),
        default_sampler: world.resource::<DefaultGltfImageSampler>().get_internal(),
        extensions: world.resource::<GltfExtensionHandlers>().0.clone(),
        default_convert_coordinates: plugin.convert_coordinates,
        default_skinned_mesh_bounds_policy: plugin.skinned_mesh_bounds_policy,
        default_mesh_compression: plugin.mesh_compression.clone(),
    }));
}

const VRMC_NODE_CONSTRAINT: &str = "VRMC_node_constraint";

/// glTF extensions this crate implements. Bevy's validator does not know them,
/// so the wrapper validates the document with these entries filtered out.
const VRMC_EXTENSIONS: [&str; 5] = [
    VRMC_VRM,
    VRMC_VRM_ANIMATION,
    VRMC_MATERIALS_MTOON,
    VRMC_SPRING_BONE,
    VRMC_NODE_CONSTRAINT,
];

fn validate_gltf(bytes: &[u8]) -> Result<(), gltf::Error> {
    use gltf::json::validation::Validate;
    // Only the JSON chunk is needed; `Gltf` would copy the BIN chunk too.
    let json = if bytes.starts_with(b"glTF") {
        gltf::binary::Glb::from_slice(bytes)?.json
    } else {
        Cow::Borrowed(bytes)
    };
    let mut root: gltf::json::Root = gltf::json::deserialize::from_slice(&json)?;
    root.extensions_required
        .retain(|extension| !VRMC_EXTENSIONS.contains(&extension.as_str()));
    let mut errors = Vec::new();
    root.validate(&root, gltf::json::Path::new, &mut |path, error| {
        errors.push((path(), error))
    });
    if errors.is_empty() {
        Ok(())
    } else {
        Err(gltf::Error::Validation(errors))
    }
}

#[derive(TypePath)]
struct VrmLoader(GltfLoader);
impl AssetLoader for VrmLoader {
    type Asset = Gltf;
    type Settings = GltfLoaderSettings;
    type Error = GltfError;
    async fn load(
        &self,
        reader: &mut dyn Reader,
        settings: &Self::Settings,
        context: &mut LoadContext<'_>,
    ) -> Result<Gltf, GltfError> {
        let motion = context.path().path().extension().and_then(|x| x.to_str()) == Some("vrma");
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        if settings.validate {
            // gltf-json rejects every `extensionsRequired` entry outside its
            // own list, including the VRMC extensions handled above. Validate
            // the document with just those entries removed; unrelated errors
            // still fail the load.
            validate_gltf(&bytes)?;
        }
        // Validation has already run above (or was explicitly disabled), so
        // both paths hand the same byte slice to the glTF loader.
        let loader_settings = GltfLoaderSettings {
            include_source: true,
            // VRM's glTF animations are unused and cameras are not
            // part of the avatar; VRMA needs its animation clips.
            load_animations: motion,
            load_cameras: false,
            default_sampler: settings.default_sampler.clone(),
            load_meshes: settings.load_meshes,
            load_materials: settings.load_materials,
            load_lights: settings.load_lights,
            override_sampler: settings.override_sampler,
            validate: false,
            convert_coordinates: settings.convert_coordinates,
            skinned_mesh_bounds_policy: settings.skinned_mesh_bounds_policy,
            mesh_compression: settings.mesh_compression.clone(),
        };
        let mut asset = GltfLoader::load_gltf(&self.0, &bytes, context, &loader_settings).await?;
        let source = asset.source.take().expect("native include_source setting");
        let scene_label = asset
            .default_scene
            .as_ref()
            .or(asset.scenes.first())
            .and_then(|h| h.path()?.label())
            .ok_or_else(|| invalid("VRM requires a scene"))?;
        let scene = context
            .get_labeled(scene_label)
            .and_then(|x| x.get::<WorldAsset>())
            .ok_or_else(|| invalid("VRM scene unavailable"))?;
        let (root, result) = scene
            .world
            .iter_entities()
            .find_map(|e| Some((e.id(), e.get::<VrmScene>()?)))
            .ok_or_else(|| invalid("VRMC scene data unavailable"))?;
        if let Some(motion) = result.0.as_ref().map_err(|e| invalid(e.to_string()))? {
            for (label, clip) in crate::animation::retarget_motion(
                &motion.source,
                &source.document,
                &motion.nodes,
                context,
                &scene.world,
                root,
            )? {
                context.add_labeled_asset(label, clip);
            }
        }
        if settings.include_source {
            asset.source = Some(source);
        }
        Ok(asset)
    }
    fn extensions(&self) -> &[&str] {
        &["vrm", "vrma"]
    }
}

pub(crate) fn invalid(
    error: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> std::io::Error {
    std::io::Error::other(error)
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

pub(crate) fn require_node(
    nodes: &HashMap<usize, Entity>,
    index: usize,
) -> std::io::Result<Entity> {
    nodes
        .get(&index)
        .copied()
        .ok_or_else(|| invalid(format!("VRMC node {index} is outside the selected scene")))
}

// Extension hooks cannot return errors; the wrapper reads this load-only result.
#[derive(Component)]
#[component(clone_behavior = Ignore)]
pub(crate) struct VrmScene(std::io::Result<Option<MotionScene>>);

struct MotionScene {
    source: Arc<schema::Motion>,
    nodes: HashMap<usize, Entity>,
}

enum Definition {
    Model(Box<schema::Model>),
    Motion(Arc<schema::Motion>),
}

#[derive(Default)]
struct VrmExtensions {
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
                if motion.spec_version != "1.0" {
                    return Err(invalid("VRMC_vrm_animation requires specVersion 1.0"));
                }
                validate_motion(&motion)?;
                return Ok(Definition::Motion(Arc::new(motion)));
            }
            let model: schema::Model = extension(source.extension_value(VRMC_VRM), "VRMC_vrm")?;
            if model.spec_version != "1.0" {
                return Err(invalid("VRMC_vrm requires specVersion 1.0"));
            }
            for material in source.materials() {
                let Some(value) = material.extension_value(VRMC_MATERIALS_MTOON) else {
                    continue;
                };
                let toon: schema::Toon = schema::Toon::deserialize(value).map_err(invalid)?;
                if toon.schema.spec_version != "1.0" {
                    return Err(invalid("VRMC_materials_mtoon requires specVersion 1.0"));
                }
                self.materials
                    .insert(material.index().expect("indexed material"), toon);
            }
            self.springs = source
                .extension_value(VRMC_SPRING_BONE)
                .map(schema::Springs::deserialize)
                .transpose()
                .map_err(invalid)?;
            if let Some(springs) = &self.springs
                && springs.spec_version != "1.0"
            {
                return Err(invalid("VRMC_springBone requires specVersion 1.0"));
            }
            for node in source.nodes() {
                let Some(value) = node.extension_value(VRMC_NODE_CONSTRAINT) else {
                    continue;
                };
                let constraint: schema::NodeConstraint =
                    extension(Some(value), VRMC_NODE_CONSTRAINT)?;
                if constraint.spec_version != "1.0" {
                    return Err(invalid("VRMC_node_constraint requires specVersion 1.0"));
                }
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
                .and_then(|info| info.texture_transform().map(schema::UvTransform::from_gltf))
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
