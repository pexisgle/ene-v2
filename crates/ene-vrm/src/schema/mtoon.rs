use super::version::Version;
use bevy::{
    math::{Affine2, Mat3},
    prelude::*,
    render::render_resource::ShaderType,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Toon {
    #[serde(rename = "specVersion")]
    pub _version: Version,
    #[serde(flatten)]
    pub params: ToonParams,
    pub shade_multiply_texture: Option<gltf::json::texture::Info>,
    pub shading_shift_texture: Option<ShadingShiftTexture>,
    pub matcap_texture: Option<gltf::json::texture::Info>,
    pub rim_multiply_texture: Option<gltf::json::texture::Info>,
    pub outline_width_multiply_texture: Option<gltf::json::texture::Info>,
    pub uv_animation_mask_texture: Option<gltf::json::texture::Info>,
    #[serde(default)]
    pub outline_width_mode: OutlineMode,
    #[serde(default)]
    pub transparent_with_z_write: bool,
    #[serde(default)]
    pub render_queue_offset_number: f32,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, ShaderType)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ToonParams {
    pub shade_color_factor: Vec3,
    pub parametric_rim_color_factor: Vec3,
    pub matcap_factor: Vec3,
    pub outline_color_factor: Vec3,
    pub shading_shift_factor: f32,
    pub shading_toony_factor: f32,
    pub gi_equalization_factor: f32,
    pub parametric_rim_fresnel_power_factor: f32,
    pub parametric_rim_lift_factor: f32,
    pub rim_lighting_mix_factor: f32,
    pub outline_width_factor: f32,
    pub outline_lighting_mix_factor: f32,
    pub uv_animation_scroll_x_speed_factor: f32,
    pub uv_animation_scroll_y_speed_factor: f32,
    pub uv_animation_rotation_speed_factor: f32,
    #[serde(skip)]
    pub shading_shift_texture_scale: f32,
    #[serde(skip)]
    pub texture_flags: u32,
    #[serde(skip)]
    pub texture_uvs: u32,
    #[serde(skip)]
    pub uv_transforms: [Mat3; 6],
}

impl Default for ToonParams {
    fn default() -> Self {
        Self {
            shade_color_factor: Vec3::ZERO,
            parametric_rim_color_factor: Vec3::ZERO,
            matcap_factor: Vec3::ONE,
            outline_color_factor: Vec3::ZERO,
            shading_shift_factor: 0.,
            shading_toony_factor: 0.9,
            gi_equalization_factor: 0.9,
            parametric_rim_fresnel_power_factor: 5.,
            parametric_rim_lift_factor: 0.,
            rim_lighting_mix_factor: 1.,
            outline_width_factor: 0.,
            outline_lighting_mix_factor: 1.,
            uv_animation_scroll_x_speed_factor: 0.,
            uv_animation_scroll_y_speed_factor: 0.,
            uv_animation_rotation_speed_factor: 0.,
            shading_shift_texture_scale: 0.,
            texture_flags: 0,
            texture_uvs: 0,
            uv_transforms: [Mat3::IDENTITY; 6],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
#[repr(u32)]
pub(crate) enum OutlineMode {
    #[default]
    None,
    WorldCoordinates = 1,
    ScreenCoordinates = 2,
}

#[derive(Deserialize)]
pub(crate) struct ShadingShiftTexture {
    #[serde(flatten)]
    pub texture: gltf::json::texture::Info,
    pub scale: Option<f32>,
}

pub(crate) fn texture_transform(
    info: &gltf::json::texture::Info,
) -> Option<&gltf::json::extensions::texture::TextureTransform> {
    info.extensions
        .as_ref()
        .and_then(|extensions| extensions.texture_transform.as_ref())
}

pub(crate) fn texture_uv(info: &gltf::json::texture::Info) -> u32 {
    texture_transform(info)
        .and_then(|transform| transform.tex_coord)
        .unwrap_or(info.tex_coord)
}

pub(crate) fn texture_affine(info: &gltf::json::texture::Info) -> Affine2 {
    let Some(transform) = texture_transform(info) else {
        return Affine2::IDENTITY;
    };
    Affine2::from_scale_angle_translation(
        Vec2::from(transform.scale.0),
        -transform.rotation.0,
        Vec2::from(transform.offset.0),
    )
}

impl Toon {
    pub fn textures(&self) -> [Option<&gltf::json::texture::Info>; 6] {
        [
            self.shade_multiply_texture.as_ref(),
            self.shading_shift_texture.as_ref().map(|s| &s.texture),
            self.rim_multiply_texture.as_ref(),
            self.matcap_texture.as_ref(),
            self.uv_animation_mask_texture.as_ref(),
            self.outline_width_multiply_texture.as_ref(),
        ]
    }
    pub fn texture_uvs(&self) -> u32 {
        let mut bits = 0;
        for (index, info) in self.textures().into_iter().enumerate() {
            if index != 3
                && let Some(info) = info
            {
                bits |= u32::from(texture_uv(info) == 1) << index;
            }
        }
        bits
    }
}
