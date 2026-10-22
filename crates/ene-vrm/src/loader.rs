use crate::{
    material::{self, MToonExtension, MToonMaterial},
    runtime, schema,
};
use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    ecs::system::RunSystemOnce,
    gltf::{
        DefaultGltfImageSampler, Gltf, GltfError, GltfLoader, GltfLoaderSettings, GltfMaterial,
        extensions::{ErasedGltfExtensionHandler, GltfExtensionHandler, GltfExtensionHandlers},
    },
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    platform::collections::HashMap,
    prelude::*,
};
use serde::Deserialize;
use std::sync::Arc;

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
    }));
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
        let mut asset = self
            .0
            .load(
                reader,
                &GltfLoaderSettings {
                    include_source: true,
                    default_sampler: settings.default_sampler.clone(),
                    ..*settings
                },
                context,
            )
            .await?;
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
            .ok_or_else(|| invalid("VRMC scene validation unavailable"))?;
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
pub(crate) fn require_node(
    nodes: &HashMap<usize, Entity>,
    index: usize,
) -> std::io::Result<Entity> {
    nodes
        .get(&index)
        .copied()
        .ok_or_else(|| invalid(format!("VRMC node {index} is outside the selected scene")))
}

fn validate_humanoid(
    humanoid: &schema::Humanoid,
    nodes: &HashMap<usize, Entity>,
    world: &mut World,
    is_motion: bool,
) -> std::io::Result<()> {
    if !humanoid.human_bones.contains_key("hips") {
        return Err(invalid("VRMC humanoid requires hips"));
    }
    let mut parent_query = world.query::<&ChildOf>();
    let parents = parent_query.query(world);
    let mut used = std::collections::HashSet::new();
    for (name, node) in &humanoid.human_bones {
        if is_motion && matches!(name.as_str(), "leftEye" | "rightEye") {
            return Err(invalid("VRMA humanoid prohibits eye bones"));
        }
        let mut parent = schema::humanoid_parent(name)
            .ok_or_else(|| invalid(format!("unknown VRMC humanoid bone {name}")))?;
        let entity = require_node(nodes, node.node)?;
        if !used.insert(node.node) {
            return Err(invalid("VRMC humanoid bones must reference distinct nodes"));
        }
        while !parent.is_empty() {
            if let Some(anc) = humanoid.human_bones.get(parent) {
                let anc_entity = require_node(nodes, anc.node)?;
                if !parents.iter_ancestors(entity).any(|e| e == anc_entity) {
                    return Err(invalid("VRMC humanoid bone violates hierarchy"));
                }
                break;
            }
            parent = schema::humanoid_parent(parent).unwrap_or("");
        }
    }
    Ok(())
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
    materials: HashMap<usize, schema::Toon>,
    springs: Option<schema::Springs>,
    nodes: HashMap<usize, Entity>,
    textures: Vec<Handle<Image>>,
    constraints: Vec<(usize, schema::NodeConstraint)>,
    thumbnail: Option<Handle<Image>>,
    thumbnail_texture: Option<usize>,
}

impl GltfExtensionHandler for VrmExtensions {
    fn dyn_clone(&self) -> Box<dyn ErasedGltfExtensionHandler> {
        Box::<Self>::default()
    }
    fn on_root(
        &mut self,
        context: &mut LoadContext<'_>,
        source: &gltf::Gltf,
        settings: &GltfLoaderSettings,
    ) {
        let suffix = context.path().path().extension().and_then(|x| x.to_str());
        let motion = suffix == Some("vrma")
            || (suffix != Some("vrm")
                && source.extension_value("VRMC_vrm").is_none()
                && source.extension_value("VRMC_vrm_animation").is_some());
        if !motion && suffix != Some("vrm") && source.extension_value("VRMC_vrm").is_none() {
            return;
        }
        self.definition = Some((|| {
            if motion {
                return Ok(Definition::Motion(Arc::new(extension(
                    source.extension_value("VRMC_vrm_animation"),
                    "VRMC_vrm_animation",
                )?)));
            }
            let model: schema::Model = extension(source.extension_value("VRMC_vrm"), "VRMC_vrm")?;
            if let Some(img) = model.meta.as_ref().and_then(|m| m.thumbnail_image) {
                let image = source
                    .images()
                    .nth(img)
                    .ok_or_else(|| invalid("VRM thumbnail image out of range"))?;
                if let Some(t) = source.textures().find(|t| t.source().index() == img) {
                    self.thumbnail_texture = Some(t.index());
                } else {
                    self.thumbnail = match image.source() {
                        gltf::image::Source::Uri { uri, .. } => Some(
                            context.load(context.path().resolve_embed_str(uri).map_err(invalid)?),
                        ),
                        gltf::image::Source::View { view, mime_type } => source
                            .blob
                            .as_deref()
                            .and_then(|b| b.get(view.offset()..view.offset() + view.length()))
                            .map(|bytes| {
                                Image::from_buffer(
                                    bytes,
                                    bevy::image::ImageType::MimeType(mime_type),
                                    CompressedImageFormats::NONE,
                                    true,
                                    bevy::image::ImageSampler::Default,
                                    settings.load_materials,
                                )
                                .map(|img| context.add_labeled_asset("VrmThumbnail", img))
                                .map_err(invalid)
                            })
                            .transpose()?,
                    };
                }
            }
            if model
                .expressions
                .materials()
                .any(|i| i >= source.materials().len())
            {
                return Err(invalid("VRM expression material out of range"));
            }
            for material in source.materials() {
                if let Some(value) = material.extension_value("VRMC_materials_mtoon") {
                    let toon = schema::Toon::deserialize(value).map_err(invalid)?;
                    if !toon.render_queue_offset_number.is_finite()
                        || !(-9. ..=9.).contains(&toon.render_queue_offset_number)
                        || toon
                            .shading_shift_texture
                            .as_ref()
                            .is_some_and(|s| s.scale.is_some_and(|sc| !sc.is_finite()))
                    {
                        return Err(invalid("MToon parameters out of range"));
                    }
                    if toon
                        .textures()
                        .into_iter()
                        .flatten()
                        .any(|t| t.index.value() >= source.textures().len())
                    {
                        return Err(invalid("MToon texture out of range"));
                    }
                    self.materials
                        .insert(material.index().expect("indexed material"), toon);
                }
            }
            self.springs = source
                .extension_value("VRMC_springBone")
                .map(schema::Springs::deserialize)
                .transpose()
                .map_err(invalid)?;
            for node in source.nodes() {
                if let Some(value) = node.extension_value("VRMC_node_constraint") {
                    self.constraints.push((
                        node.index(),
                        extension(Some(value), "VRMC_node_constraint")?,
                    ));
                }
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
        let Some(toon) = source.index().and_then(|i| self.materials.get(&i)) else {
            return;
        };
        let Some(mut base) = context
            .get_labeled(format!("{label}/std"))
            .and_then(|a| a.get::<StandardMaterial>())
            .cloned()
        else {
            return;
        };
        base.opaque_render_method = bevy::material::OpaqueRendererMethod::Forward;
        if base.alpha_mode == AlphaMode::Blend {
            base.depth_bias += toon.render_queue_offset_number
                + if toon.transparent_with_z_write {
                    -10.
                } else {
                    0.
                };
        }
        let material = MToonMaterial {
            base,
            extension: MToonExtension::new(toon, &self.textures),
        };
        let label = format!("{label}/vrm");
        if toon.outline_width_mode != schema::OutlineMode::None {
            let mut outline = material.clone();
            outline.base.double_sided = false;
            if outline.base.cull_mode.is_none() && label.ends_with("/inverted/vrm") {
                outline.base.cull_mode = Some(bevy::render::render_resource::Face::Front);
            }
            outline.extension.outline = Some(toon.outline_width_mode);
            context.add_labeled_asset(format!("{label}/outline"), outline);
        }
        context.add_labeled_asset(label, material);
    }
    fn on_spawn_mesh_and_material(
        &mut self,
        context: &mut LoadContext<'_>,
        _: &gltf::Primitive,
        _: &gltf::Mesh,
        source: &gltf::Material,
        entity: &mut EntityWorldMut,
        label: &str,
    ) {
        if !matches!(self.definition, Some(Ok(Definition::Model(_)))) {
            return;
        }
        if let Some(index) = source.index() {
            entity.insert(material::MaterialSlot(index));
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
        _: &gltf::Scene,
        root: Entity,
        world: &mut World,
    ) {
        let Some(definition) = &self.definition else {
            return;
        };
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
                            .filter(|toon| toon.outline_width_mode != schema::OutlineMode::None)?;
                        Some(
                            context
                                .get_label_handle(format!("{}/outline", handle.path()?.label()?)),
                        )
                    });
                    validate_humanoid(&model.humanoid, &self.nodes, world, false)?;
                    for a in &model.first_person.mesh_annotations {
                        require_node(&self.nodes, a.node)?;
                    }
                    if let Some(g) = &model.look_at
                        && (!g.offset_from_head_bone.is_finite()
                            || [
                                g.horizontal_inner(),
                                g.horizontal_outer(),
                                g.vertical_down(),
                                g.vertical_up(),
                            ]
                            .into_iter()
                            .any(|r| {
                                !r.input_max_value.is_finite()
                                    || r.input_max_value <= 0.
                                    || !r.output_scale.is_finite()
                                    || r.output_scale < 0.
                            }))
                    {
                        return Err(invalid("VRM lookAt contains invalid range map or offset"));
                    }
                    let thumbnail = self.thumbnail.clone().or_else(|| {
                        self.thumbnail_texture
                            .and_then(|i| self.textures.get(i).cloned())
                    });
                    runtime::attach(
                        world,
                        root,
                        model,
                        thumbnail,
                        self.springs.as_ref(),
                        &self.constraints,
                        &self.nodes,
                    )?;
                    Ok(None)
                }
                Definition::Motion(motion) => {
                    if let Some(humanoid) = &motion.humanoid {
                        validate_humanoid(humanoid, &self.nodes, world, true)?;
                    }
                    if motion.expressions.preset.keys().any(|k| {
                        matches!(
                            k.as_str(),
                            "lookUp" | "lookDown" | "lookLeft" | "lookRight" | "neutral"
                        )
                    }) {
                        return Err(invalid("VRMA preset expression is prohibited"));
                    }
                    if let Some(l) = &motion.look_at
                        && !l.offset_from_head_bone.is_finite()
                    {
                        return Err(invalid("VRMA lookAt offset must be finite"));
                    }
                    for &node in motion
                        .expressions
                        .iter()
                        .map(|(_, n)| &n.node)
                        .chain(motion.look_at.as_ref().and_then(|l| l.node.as_ref()))
                    {
                        require_node(&self.nodes, node)?;
                    }
                    Ok(Some(MotionScene {
                        source: motion.clone(),
                        nodes: std::mem::take(&mut self.nodes),
                    }))
                }
            }
        })();
        world.entity_mut(root).insert(VrmScene(result));
        self.nodes.clear();
    }
}
