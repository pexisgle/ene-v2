use crate::schema::{OutlineMode, Toon, ToonParams, UvTransform};
use bevy::{
    asset::embedded_asset,
    camera::visibility::NoFrustumCulling,
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

/// Authored `KHR_texture_transform` of the material's base color texture.
///
/// `StandardMaterial::uv_transform` is a matrix, which cannot represent a
/// rotation once an axis has zero scale. Expressions keep the source
/// parameters so a zero or negative scale still blends correctly.
#[derive(Component, Clone, Copy, Debug, Default, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct BaseUv(pub UvTransform);

/// `KHR_materials_emissive_strength` of the glTF material.
///
/// Bevy folds the strength into [`StandardMaterial::emissive`] while loading,
/// but expression binds target the raw `emissiveFactor`. Keep the factor so a
/// bind can apply it to its target as well instead of interpolating toward an
/// unscaled color.
#[derive(Component, Clone, Copy, Debug, Reflect)]
#[reflect(Component, Clone)]
pub(crate) struct EmissiveStrength(pub f32);

impl Default for EmissiveStrength {
    fn default() -> Self {
        Self(1.)
    }
}

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
    fn enable_oit() -> bool {
        false
    }
    fn vertex_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/outline.wesl".into()
    }
    fn fragment_shader() -> ShaderRef {
        "embedded://ene_vrm/material/shaders/mtoon.wesl".into()
    }
    // ponytail: forward-only. Bevy's standard prepass would not evaluate the
    // animated alpha mask; re-add a matching prepass and shadow sampling if
    // those consumers become a requirement (see README「対象外と制限」).
    fn enable_prepass() -> bool {
        false
    }
    fn enable_shadows() -> bool {
        false
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
        if key.bind_group_data.outline.is_none() {
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
        let mut params = ToonParams::from(&toon.schema);
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
    embedded_asset!(app, "shaders/mtoon.wesl");
    embedded_asset!(app, "shaders/outline.wesl");
    bevy::shader::load_shader_library!(app, "shaders/toon.wesl");
    app.add_plugins(MaterialPlugin::<MToonMaterial>::default())
        .register_type::<MaterialSlot>()
        .register_type::<BaseUv>()
        .register_type::<EmissiveStrength>()
        .register_type::<UvTransform>();
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
        // is despawned with it.
        let outline = world
            .spawn((
                MeshMaterial3d(material),
                NoFrustumCulling,
                Transform::IDENTITY,
                ChildOf(entity),
            ))
            .id();
        world.entity_mut(entity).clone_components::<(
            Mesh3d,
            SkinnedMesh,
            MeshMorphWeights,
            MaterialSlot,
            BaseUv,
            EmissiveStrength,
        )>(outline);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VrmPlugin, schema};
    use bevy::{
        asset::RenderAssetUsages,
        mesh::{
            VertexAttributeValues, morph::MorphAttributes, skinning::SkinnedMeshInverseBindposes,
        },
        render::render_resource::{Extent3d, TextureDimension, TextureFormat},
    };
    use serde_json::json;
    use std::time::Duration;

    fn assert_specialized_pipelines_ok(app: &App, entities: &[Entity]) {
        use bevy::pbr::SpecializedMaterialPipelineCache;
        use bevy::render::{
            RenderApp,
            render_resource::{CachedPipelineState, PipelineCache},
            sync_world::MainEntity,
        };
        let world = app.sub_app(RenderApp).world();
        let cache = world.resource::<SpecializedMaterialPipelineCache>();
        let pipelines = world.resource::<PipelineCache>();
        for entity in entities {
            let id = cache
                .values()
                .find_map(|view| view.get(&MainEntity::from(*entity)))
                .copied()
                .unwrap_or_else(|| panic!("{entity:?} has no specialized material pipeline"));
            assert!(
                matches!(
                    pipelines.get_render_pipeline_state(id),
                    CachedPipelineState::Ok(_)
                ),
                "{entity:?} pipeline was not created"
            );
        }
    }

    /// Headless app that actually renders: the render plugin stays on, no window is
    /// created, pipelines compile synchronously, and time advances by 1/60 s per
    /// update so the synthetic GPU tests are deterministic.
    fn gpu_app(clear: ClearColor) -> App {
        use bevy::render::RenderPlugin;
        let mut plugins = DefaultPlugins.build();
        if plugins.contains::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>() {
            plugins =
                plugins.disable::<bevy::render::pipelined_rendering::PipelinedRenderingPlugin>();
        }
        #[cfg(windows)]
        let plugins = plugins.disable::<bevy::winit::WinitPlugin>();
        let mut app = App::new();
        app.add_plugins(
            plugins
                .set(WindowPlugin {
                    primary_window: None,
                    exit_condition: bevy::window::ExitCondition::DontExit,
                    ..default()
                })
                .set(RenderPlugin {
                    synchronous_pipeline_compilation: true,
                    ..default()
                }),
        )
        .add_plugins(VrmPlugin)
        .insert_resource(clear)
        .insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_secs_f64(1. / 60.),
        ));
        while app.plugins_state() != bevy::app::PluginsState::Ready {
            bevy::tasks::tick_global_task_pools_on_main_thread();
            std::thread::yield_now();
        }
        app.finish();
        app.cleanup();
        app
    }

    #[test]
    #[ignore = "requires a GPU; runs in CI with lavapipe"]
    fn mtoon_diffuse_preserves_authored_color_under_unit_lighting() {
        use bevy::{
            camera::{Exposure, RenderTarget, ScalingMode},
            core_pipeline::tonemapping::Tonemapping,
            render::view::screenshot::{Screenshot, ScreenshotCaptured},
        };

        #[derive(Resource, Default)]
        struct Capture(Option<Image>);

        let mut app = gpu_app(ClearColor(Color::NONE));
        app.init_resource::<Capture>();
        let color = Color::linear_rgb(0.25, 0.5, 0.75);
        let toon: schema::Toon = serde_json::from_value(json!({"specVersion":"1.0"})).unwrap();
        let material = app
            .world_mut()
            .resource_mut::<Assets<MToonMaterial>>()
            .add(MToonMaterial {
                base: StandardMaterial {
                    base_color: color,
                    ..default()
                },
                extension: MToonExtension::new(&toon, &[], &HashMap::default()),
            });
        let mesh = app
            .world_mut()
            .resource_mut::<Assets<Mesh>>()
            .add(Rectangle::new(1., 1.));
        app.world_mut()
            .spawn((Mesh3d(mesh), MeshMaterial3d(material)));
        let target =
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .add(Image::new_target_texture(
                    32,
                    32,
                    TextureFormat::Rgba8UnormSrgb,
                    None,
                ));
        app.world_mut().spawn((
            Camera3d::default(),
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: 2.,
                    height: 2.,
                },
                ..OrthographicProjection::default_3d()
            }),
            RenderTarget::Image(target.clone().into()),
            Transform::from_xyz(0., 0., 3.),
            Exposure::BLENDER,
            Tonemapping::Linear,
            Msaa::Off,
        ));
        let light = app.world_mut().spawn(DirectionalLight::default()).id();

        // Each combination has a unit diffuse response. Before the Lambert
        // correction, direct light clips green and blue instead of preserving
        // the authored color. Ambient is already integrated by Bevy.
        for direct in [1., 0., 0.8] {
            app.world_mut()
                .get_mut::<DirectionalLight>(light)
                .unwrap()
                .illuminance = direct * std::f32::consts::PI / Exposure::BLENDER.exposure();
            app.insert_resource(GlobalAmbientLight {
                color: Color::WHITE,
                brightness: (1. - direct) / Exposure::BLENDER.exposure(),
                ..default()
            });
            for _ in 0..30 {
                app.update();
            }
            app.world_mut()
                .spawn(Screenshot::image(target.clone()))
                .observe(
                    |event: On<ScreenshotCaptured>, mut capture: ResMut<Capture>| {
                        capture.0 = Some(event.image.clone());
                    },
                );
            let image = (0..1000)
                .find_map(|_| {
                    app.update();
                    app.world_mut().resource_mut::<Capture>().0.take()
                })
                .expect("GPU screenshot did not complete")
                .try_into_dynamic()
                .unwrap()
                .to_rgba8();
            let actual = image.get_pixel(16, 16).0;
            let expected = color.to_srgba().to_u8_array();
            for channel in 0..4 {
                assert!(
                    actual[channel].abs_diff(expected[channel]) <= 2,
                    "direct={direct}: rendered {actual:?}, expected {expected:?}"
                );
            }
            assert_eq!(image.get_pixel(0, 0).0[3], 0, "transparent background");
        }
    }

    #[test]
    #[ignore = "requires a GPU; runs in CI with lavapipe"]
    fn synthetic_mtoon_uv1_outline_pipeline_smoke() {
        use bevy::mesh::PrimitiveTopology;
        use bevy::render::{
            RenderApp,
            render_resource::{CachedPipelineState, PipelineCache},
        };
        let mut app = gpu_app(ClearColor(Color::NONE));

        let color_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::new(
                Extent3d {
                    width: 2,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                vec![
                    255, 0, 0, 255, // red
                    0, 255, 0, 255, // green
                    0, 0, 255, 255, // blue
                    255, 255, 0, 255, // yellow
                ],
                TextureFormat::Rgba8UnormSrgb,
                RenderAssetUsages::default(),
            ));
        let mask_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::new(
                Extent3d {
                    width: 2,
                    height: 2,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                vec![0, 255, 0, 255, 0, 64, 0, 255, 0, 0, 0, 128, 0, 192, 0, 255],
                TextureFormat::Rgba8Unorm,
                RenderAssetUsages::default(),
            ));
        let normal_image = app
            .world_mut()
            .resource_mut::<Assets<Image>>()
            .add(Image::new(
                Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                vec![128, 128, 255, 255],
                TextureFormat::Rgba8Unorm,
                RenderAssetUsages::default(),
            ));
        let converted = HashMap::from([((0usize, TextureSpace::Linear), mask_image)]);
        let toon: schema::Toon = serde_json::from_value(json!({
            "specVersion":"1.0",
            "shadeMultiplyTexture":{"index":0,"texCoord":1},
            "uvAnimationMaskTexture":{"index":0,"texCoord":1},
            "outlineWidthMode":"screenCoordinates",
            "outlineWidthFactor":0.01,
            "outlineWidthMultiplyTexture":{"index":0,"texCoord":1}
        }))
        .unwrap();
        let body_material =
            app.world_mut()
                .resource_mut::<Assets<MToonMaterial>>()
                .add(MToonMaterial {
                    base: StandardMaterial {
                        alpha_mode: bevy::material::AlphaMode::Mask(0.5),
                        base_color_texture: Some(color_image.clone()),
                        normal_map_texture: Some(normal_image),
                        ..default()
                    },
                    extension: MToonExtension::new(
                        &toon,
                        std::slice::from_ref(&color_image),
                        &converted,
                    ),
                });
        let mut outline_extension =
            MToonExtension::new(&toon, std::slice::from_ref(&color_image), &converted);
        outline_extension.outline = Some(schema::OutlineMode::ScreenCoordinates);
        let outline_material =
            app.world_mut()
                .resource_mut::<Assets<MToonMaterial>>()
                .add(MToonMaterial {
                    base: StandardMaterial {
                        cull_mode: Some(bevy::render::render_resource::Face::Back),
                        ..default()
                    },
                    extension: outline_extension,
                });

        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_POSITION,
            vec![[-1., -1., 0.], [1., -1., 0.], [0., 1., 0.]],
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, vec![[0., 0., 1.]; 3]);
        // UV0 and UV1 differ so the MToon texture coordinate selection is
        // exercised instead of comparing equal coordinates.
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, vec![[0., 0.], [1., 0.], [0., 1.]]);
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_1, vec![[0., 0.], [0., 1.], [1., 0.]]);
        // Exercise the tangent/normal-map, skinning and morph shader variants.
        mesh.insert_attribute(Mesh::ATTRIBUTE_TANGENT, vec![[1., 0., 0., 1.]; 3]);
        mesh.insert_attribute(
            Mesh::ATTRIBUTE_JOINT_INDEX,
            VertexAttributeValues::Uint16x4(vec![[0u16; 4]; 3]),
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, vec![[1., 0., 0., 0.]; 3]);
        mesh.set_morph_targets(vec![
            MorphAttributes::new(Vec3::ZERO, Vec3::ZERO, Vec3::ZERO);
            3
        ]);
        let mesh = app.world_mut().resource_mut::<Assets<Mesh>>().add(mesh);
        let joint = app.world_mut().spawn(Transform::default()).id();
        let inverse_bindposes = app
            .world_mut()
            .resource_mut::<Assets<SkinnedMeshInverseBindposes>>()
            .add(SkinnedMeshInverseBindposes::from(vec![Mat4::IDENTITY]));
        let skin = SkinnedMesh {
            inverse_bindposes,
            joints: vec![joint],
        };
        let morph = MeshMorphWeights::Value { weights: vec![0.5] };
        let body = app
            .world_mut()
            .spawn((
                Mesh3d(mesh.clone()),
                MeshMaterial3d(body_material),
                Visibility::Visible,
                NoFrustumCulling,
                skin.clone(),
                morph.clone(),
            ))
            .id();
        app.world_mut().spawn((
            Mesh3d(mesh),
            MeshMaterial3d(outline_material),
            Visibility::Visible,
            NoFrustumCulling,
            Transform::IDENTITY,
            ChildOf(body),
            skin,
            morph,
        ));

        let target =
            app.world_mut()
                .resource_mut::<Assets<Image>>()
                .add(Image::new_target_texture(
                    64,
                    64,
                    TextureFormat::Rgba8UnormSrgb,
                    None,
                ));
        app.world_mut().spawn((
            Camera3d::default(),
            bevy::core_pipeline::tonemapping::Tonemapping::Linear,
            bevy::camera::RenderTarget::Image(target.into()),
            Transform::from_xyz(0., 0., 3.).looking_at(Vec3::ZERO, Vec3::Y),
        ));
        app.world_mut().spawn((
            DirectionalLight::default(),
            Transform::from_rotation(Quat::from_euler(EulerRot::XYZ, -0.4, -0.3, 0.)),
        ));
        for _ in 0..30 {
            app.update();
        }
        for pipeline in app
            .sub_app(RenderApp)
            .world()
            .resource::<PipelineCache>()
            .pipelines()
        {
            assert!(
                !matches!(pipeline.state, CachedPipelineState::Err(_)),
                "{:?}",
                pipeline.state
            );
        }
        let entities: Vec<Entity> = app
            .world_mut()
            .query_filtered::<Entity, With<MeshMaterial3d<MToonMaterial>>>()
            .iter(app.world())
            .collect();
        assert_eq!(entities.len(), 2, "body and outline materials");
        assert_specialized_pipelines_ok(&app, &entities);
    }
}
