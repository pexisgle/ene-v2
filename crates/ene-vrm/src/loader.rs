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
fn render_queue_bias(offset: f32, z_write: bool) -> f32 {
    offset + if z_write { -20. } else { 0. }
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
    constraints: Vec<(usize, schema::NodeConstraint)>,
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
                    source.extension_value("VRMC_vrm_animation"),
                    "VRMC_vrm_animation",
                )?;
                return Ok(Definition::Motion(Arc::new(motion)));
            }
            let model: schema::Model = extension(source.extension_value("VRMC_vrm"), "VRMC_vrm")?;
            for material in source.materials() {
                if let Some(value) = material.extension_value("VRMC_materials_mtoon") {
                    self.materials.insert(
                        material.index().expect("indexed material"),
                        schema::Toon::deserialize(value).map_err(invalid)?,
                    );
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
            base.depth_bias += render_queue_bias(
                toon.render_queue_offset_number,
                toon.transparent_with_z_write,
            );
        }
        let material = MToonMaterial {
            base,
            extension: MToonExtension::new(toon, &self.textures),
        };
        let label = format!("{label}/vrm");
        if toon.outline_width_mode != schema::OutlineMode::None {
            let mut outline = material.clone();
            outline.base.double_sided = false;
            if outline.base.cull_mode.is_none() && label.ends_with("(inverted)/vrm") {
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
                            .filter(|toon| toon.outline_width_mode != schema::OutlineMode::None)?;
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

#[cfg(test)]
mod tests {
    use super::render_queue_bias;

    #[test]
    fn render_queue_offset_groups_are_disjoint() {
        assert!(render_queue_bias(9., true) < render_queue_bias(-9., false));
        assert!(render_queue_bias(0., true) < render_queue_bias(0., false));
    }
}
