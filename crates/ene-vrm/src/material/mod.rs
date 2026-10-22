use crate::schema::{OutlineMode, Toon, ToonParams};
use bevy::{
    asset::embedded_asset,
    camera::visibility::NoFrustumCulling,
    light::NotShadowCaster,
    mesh::{MeshVertexBufferLayoutRef, morph::MeshMorphWeights, skinning::SkinnedMesh},
    pbr::{ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline},
    prelude::*,
    render::render_resource::{
        AsBindGroup, Face, RenderPipelineDescriptor, SpecializedMeshPipelineError,
    },
    shader::ShaderRef,
};

pub type MToonMaterial = ExtendedMaterial<StandardMaterial, MToonExtension>;

#[derive(Component, Clone, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct MaterialSlot(pub usize);

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
    pub(crate) z_write: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ToonKey {
    outline: Option<OutlineMode>,
    z_write: bool,
}
impl From<&MToonExtension> for ToonKey {
    fn from(x: &MToonExtension) -> Self {
        Self {
            outline: x.outline,
            z_write: x.z_write,
        }
    }
}

impl MaterialExtension for MToonExtension {
    fn vertex_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/outline.wgsl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/mtoon.wgsl".into()
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
            descriptor.primitive.cull_mode = match descriptor.primitive.cull_mode {
                Some(Face::Front) => Some(Face::Back),
                _ => Some(Face::Front),
            };
            if let Some(frag) = &mut descriptor.fragment {
                frag.shader_defs.push("OUTLINE_PASS".into());
            }
        }
        if key.bind_group_data.z_write
            && let Some(depth) = &mut descriptor.depth_stencil
        {
            depth.depth_write_enabled = Some(true);
        }
        Ok(())
    }
}

impl MToonExtension {
    pub(crate) fn new(toon: &Toon, images: &[Handle<Image>]) -> Self {
        let textures = toon.textures();
        let img = |i: usize| {
            textures[i]
                .and_then(|t| images.get(t.index.value()))
                .cloned()
        };
        Self {
            params: ToonParams {
                texture_flags: textures
                    .iter()
                    .enumerate()
                    .fold(0, |f, (i, t)| f | (u32::from(t.is_some()) << i)),
                shading_shift_texture_scale: toon
                    .shading_shift_texture
                    .as_ref()
                    .and_then(|x| x.scale)
                    .unwrap_or(1.),
                ..toon.params
            },
            shade: img(0),
            shift: img(1),
            rim: img(2),
            matcap: img(3),
            uv_mask: img(4),
            outline_width: img(5),
            outline: None,
            z_write: toon.transparent_with_z_write,
        }
    }
}

pub(crate) fn setup(app: &mut App) {
    embedded_asset!(app, "shaders/mtoon.wgsl");
    embedded_asset!(app, "shaders/outline.wgsl");
    bevy::shader::load_shader_library!(app, "shaders/toon_bindings.wgsl");
    app.add_plugins(MaterialPlugin::<MToonMaterial>::default())
        .register_type::<MaterialSlot>();
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
        let outline = world
            .spawn((MeshMaterial3d(material), NotShadowCaster, NoFrustumCulling))
            .id();
        world
            .entity_mut(entity)
            .clone_components::<(Mesh3d, ChildOf, SkinnedMesh, MeshMorphWeights, MaterialSlot)>(
                outline,
            );
    }
}
