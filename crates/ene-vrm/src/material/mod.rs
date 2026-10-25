use crate::schema::{OutlineMode, Toon, ToonParams, UvTransform};
use bevy::{
    asset::embedded_asset,
    camera::visibility::{NoFrustumCulling, RenderLayers},
    light::NotShadowCaster,
    mesh::{MeshVertexBufferLayoutRef, morph::MeshMorphWeights, skinning::SkinnedMesh},
    pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline},
    platform::collections::HashMap,
    prelude::*,
    render::render_resource::{
        AsBindGroup, Face, RenderPipelineDescriptor, SpecializedMeshPipelineError,
    },
    shader::ShaderRef,
};

pub type MToonMaterial = ExtendedMaterial<StandardMaterial, MToonExtension>;

/// MToon texture slots that store numbers instead of colors: shading shift,
/// UV animation mask and outline width.
pub(crate) const NUMERIC_SLOTS: [usize; 3] = [1, 4, 5];

/// MToon texture slots that store colors: shade multiply, rim multiply and
/// matcap.
pub(crate) const COLOR_SLOTS: [usize; 3] = [0, 2, 3];

/// Sampling space a texture is used in. Bevy picks the loaded image format
/// from the core glTF slots, which can disagree with an MToon slot sharing the
/// same texture index; the loader keeps a converted image per space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TextureSpace {
    Linear,
    Srgb,
}

pub(crate) fn texture_space(slot: usize) -> TextureSpace {
    if NUMERIC_SLOTS.contains(&slot) {
        TextureSpace::Linear
    } else {
        TextureSpace::Srgb
    }
}

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct MaterialSlot(pub usize);

/// Marks an entity spawned as the outline pass of another mesh entity.
#[derive(Component, Clone, Copy, Debug, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct Outline;

/// Authored `KHR_texture_transform` of the material's base color texture.
///
/// `StandardMaterial::uv_transform` is a matrix, which cannot represent a
/// rotation once an axis has zero scale. Expressions keep the source
/// parameters so a zero or negative scale still blends correctly.
#[derive(Component, Clone, Copy, Debug, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct BaseUv(pub UvTransform);

#[derive(Asset, TypePath, AsBindGroup, Clone, Debug)]
#[bind_group_data(ToonKey)]
pub struct MToonExtension {
    #[uniform(100)]
    pub(crate) params: ToonParams,
    #[texture(101)]
    #[sampler(102)]
    pub(crate) shade: Option<Handle<Image>>,
    #[texture(103)]
    #[sampler(104)]
    pub(crate) shift: Option<Handle<Image>>,
    #[texture(105)]
    #[sampler(106)]
    pub(crate) rim: Option<Handle<Image>>,
    #[texture(107)]
    #[sampler(108)]
    pub(crate) matcap: Option<Handle<Image>>,
    #[texture(109)]
    #[sampler(110)]
    pub(crate) uv_mask: Option<Handle<Image>>,
    #[texture(111)]
    #[sampler(112)]
    pub(crate) outline_width: Option<Handle<Image>>,
    pub(crate) outline: Option<OutlineMode>,
    /// Authored transforms of the six MToon textures, for expression blending.
    pub(crate) rest_uv: [UvTransform; 6],
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ToonKey {
    outline: Option<OutlineMode>,
}
impl From<&MToonExtension> for ToonKey {
    fn from(x: &MToonExtension) -> Self {
        Self { outline: x.outline }
    }
}

fn outline_cull(cull: Option<Face>) -> Option<Face> {
    match cull {
        Some(Face::Front) => Some(Face::Back),
        _ => Some(Face::Front),
    }
}

impl MaterialExtension for MToonExtension {
    fn vertex_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/outline.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/mtoon.wgsl".into()
    }
    fn prepass_fragment_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/mtoon_prepass.wgsl".into()
    }
    fn specialize(
        pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _: &MeshVertexBufferLayoutRef,
        key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        descriptor
            .vertex
            .shader_defs
            .retain(|d| d != &"VERTEX_COLORS".into());
        if let Some(frag) = &mut descriptor.fragment {
            frag.shader_defs.retain(|d| d != &"VERTEX_COLORS".into());
        }
        let has_uv = descriptor
            .vertex
            .shader_defs
            .contains(&"VERTEX_UVS_A".into())
            || descriptor
                .vertex
                .shader_defs
                .contains(&"VERTEX_UVS_B".into());
        if has_uv {
            descriptor.vertex.shader_defs.push("MTOON_UVS".into());
            if let Some(frag) = &mut descriptor.fragment {
                frag.shader_defs.push("MTOON_UVS".into());
            }
        }
        if key.bind_group_data.outline.is_none()
            && !descriptor
                .vertex
                .shader_defs
                .contains(&"PREPASS_PIPELINE".into())
        {
            descriptor.vertex.shader = pipeline.mesh_pipeline.shader.clone();
        }
        if let Some(mode) = key.bind_group_data.outline {
            if mode == OutlineMode::ScreenCoordinates {
                descriptor
                    .vertex
                    .shader_defs
                    .push("SCREEN_SPACE_OUTLINE".into());
            }
            descriptor.primitive.cull_mode = outline_cull(descriptor.primitive.cull_mode);
            if let Some(frag) = &mut descriptor.fragment {
                frag.shader_defs.push("OUTLINE_PASS".into());
            }
        }
        Ok(())
    }
}

impl MToonExtension {
    pub(crate) fn new(
        toon: &Toon,
        images: &[Handle<Image>],
        converted: &HashMap<(usize, TextureSpace), Handle<Image>>,
    ) -> Self {
        let textures = toon.textures();
        let img = |slot: usize| {
            let index = textures[slot]?.index.value();
            converted
                .get(&(index, texture_space(slot)))
                .or_else(|| images.get(index))
                .cloned()
        };
        let mut params = toon.params;
        params.texture_flags = textures
            .iter()
            .enumerate()
            .fold(0, |flags, (i, t)| flags | (u32::from(t.is_some()) << i));
        params.shading_shift_texture_scale = toon
            .shading_shift_texture
            .as_ref()
            .and_then(|x| x.scale)
            .unwrap_or(1.);
        params.texture_uvs = toon.texture_uvs();
        let mut rest_uv = [UvTransform::default(); 6];
        for (index, info) in textures.into_iter().enumerate() {
            if index != 3
                && let Some(info) = info
            {
                rest_uv[index] = UvTransform::from_json(info);
                params.uv_transforms[index] = rest_uv[index].affine().into();
            }
        }
        Self {
            params,
            shade: img(0),
            shift: img(1),
            rim: img(2),
            matcap: img(3),
            uv_mask: img(4),
            outline_width: img(5),
            outline: None,
            rest_uv,
        }
    }
}

pub(crate) fn setup(app: &mut App) {
    embedded_asset!(app, "shaders/mtoon.wgsl");
    embedded_asset!(app, "shaders/outline.wgsl");
    embedded_asset!(app, "shaders/mtoon_prepass.wgsl");
    bevy::shader::load_shader_library!(app, "shaders/toon_bindings.wgsl");
    app.add_plugins(MaterialPlugin::<MToonMaterial>::default())
        .register_type::<MaterialSlot>()
        .register_type::<BaseUv>()
        .register_type::<UvTransform>()
        .register_type::<Outline>()
        .add_systems(PostUpdate, sync_outline_layers);
}

/// `RenderLayers` is per-entity, not inherited through `ChildOf`. Copy it from
/// the body so an outline follows the same camera as the primitive at runtime.
fn sync_outline_layers(
    mut outlines: Query<(&ChildOf, &mut RenderLayers), With<Outline>>,
    sources: Query<&RenderLayers, Without<Outline>>,
) {
    for (parent, mut layers) in &mut outlines {
        let next = sources.get(parent.parent()).cloned().unwrap_or_default();
        if *layers != next {
            *layers = next;
        }
    }
}

pub(crate) fn add_outlines(
    world: &mut World,
    materials: &mut impl FnMut(usize, &Handle<MToonMaterial>) -> Option<Handle<MToonMaterial>>,
) {
    let meshes: Vec<_> = world
        .query::<(Entity, &MeshMaterial3d<MToonMaterial>, &MaterialSlot)>()
        .iter(world)
        .filter_map(|(e, mat, slot)| Some((e, materials(slot.0, &mat.0)?)))
        .collect();
    for (entity, material) in meshes {
        // A child (identity transform) inherits the primitive's visibility and
        // is despawned with it. `RenderLayers` is copied by `sync_outline_layers`.
        let outline = world
            .spawn((
                MeshMaterial3d(material),
                NotShadowCaster,
                NoFrustumCulling,
                Outline,
                RenderLayers::default(),
                Transform::IDENTITY,
                ChildOf(entity),
            ))
            .id();
        world
            .entity_mut(entity)
            .clone_components::<(Mesh3d, SkinnedMesh, MeshMorphWeights, MaterialSlot, BaseUv)>(
                outline,
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outline_cull_flips_the_base_face() {
        assert_eq!(outline_cull(None), Some(Face::Front));
        assert_eq!(outline_cull(Some(Face::Back)), Some(Face::Front));
        assert_eq!(outline_cull(Some(Face::Front)), Some(Face::Back));
    }

    #[test]
    fn mtoon_texture_uvs_and_transforms_are_collected() {
        let toon: Toon = serde_json::from_value(json!({
            "specVersion":"1.0",
            "shadeMultiplyTexture":{
                "index":0,
                "texCoord":1,
                "extensions":{"KHR_texture_transform":{
                    "offset":[0.1,0.2],
                    "rotation":0.3,
                    "scale":[2.,3.]
                }}
            },
            "shadingShiftTexture":{"index":1,"texCoord":1},
            "rimMultiplyTexture":{"index":2}
        }))
        .unwrap();
        let extension = MToonExtension::new(&toon, &[], &Default::default());
        assert_eq!(extension.params.texture_uvs, 0b11);
        let expected: bevy::math::Mat3 =
            UvTransform::from_json(toon.shade_multiply_texture.as_ref().unwrap())
                .affine()
                .into();
        assert_eq!(extension.params.uv_transforms[0], expected);
        assert_eq!(
            extension.params.uv_transforms[1],
            UvTransform::from_json(&toon.shading_shift_texture.as_ref().unwrap().texture)
                .affine()
                .into()
        );
        assert_eq!(extension.rest_uv[0].scale, Vec2::new(2., 3.));
    }
}
