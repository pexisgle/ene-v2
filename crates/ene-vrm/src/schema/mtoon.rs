use super::version::Version;
use bevy::{prelude::*, render::render_resource::ShaderType};
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
}

impl Default for ToonParams {
    fn default() -> Self {
        Self {
            shade_color_factor: Vec3::ONE,
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
}
