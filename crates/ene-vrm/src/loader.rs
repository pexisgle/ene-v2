use crate::{
    extensions::{VRMC_NODE_CONSTRAINT, VrmExtensions},
    schema,
};
use bevy::{
    asset::{AssetLoader, LoadContext, io::Reader},
    gltf::{
        DefaultGltfImageSampler, Gltf, GltfError, GltfLoader, GltfLoaderSettings,
        extensions::GltfExtensionHandlers,
    },
    image::{CompressedImageFormatSupport, CompressedImageFormats},
    platform::collections::HashMap,
    prelude::*,
};
use std::{borrow::Cow, sync::Arc};
use vrm_spec::{
    vrmc_materials_mtoon_1_0::VRMC_MATERIALS_MTOON, vrmc_spring_bone_1_0::VRMC_SPRING_BONE,
    vrmc_vrm_1_0::VRMC_VRM, vrmc_vrm_animation_1_0::VRMC_VRM_ANIMATION,
};

pub(crate) fn setup(app: &mut App) {
    app.preregister_asset_loader::<VrmLoader>(&["vrm", "vrma"]);
    let Some(handlers) = app.world().get_resource::<GltfExtensionHandlers>() else {
        panic!("VrmPlugin requires GltfPlugin");
    };
    handlers
        .0
        .write_blocking()
        .push(Box::<VrmExtensions>::default());
}

pub(crate) fn finish(app: &mut App) {
    let Some(plugin) = app
        .get_added_plugins::<bevy::gltf::GltfPlugin>()
        .first()
        .copied()
    else {
        panic!("VrmPlugin requires GltfPlugin");
    };
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
        let source = asset
            .source
            .take()
            .ok_or_else(|| invalid("native include_source setting missing"))?;
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
            for (label, clip) in crate::retarget::retarget_motion(
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
pub(crate) struct VrmScene(pub(crate) std::io::Result<Option<MotionScene>>);

pub(crate) struct MotionScene {
    pub(crate) source: Arc<schema::Motion>,
    pub(crate) nodes: HashMap<usize, Entity>,
}
