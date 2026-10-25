use bevy::{
    math::{Affine2, Mat3},
    prelude::*,
    render::render_resource::ShaderType,
};
use serde::Deserialize;
use vrm_spec::vrmc_materials_mtoon_1_0::{OutlineWidthMode, VrmcMaterialsMtoonSchema};

/// `VRMC_materials_mtoon` as authored.
///
/// `vrm-spec` owns the parameter and texture fields. Only `shadingShiftTexture`
/// is parsed locally: `vrm-spec` types that slot without the
/// `KHR_texture_transform` this crate needs for UV animation, while the other
/// five slots already use `gltf`'s typed texture info.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Toon {
    #[serde(flatten)]
    pub schema: VrmcMaterialsMtoonSchema,
    pub shading_shift_texture: Option<ShadingShiftTexture>,
}

/// Values uploaded to the shader. Defaults match three-vrm.
#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
pub(crate) struct ToonParams {
    pub shade_color_factor: Vec3,
    pub parametric_rim_color_factor: Vec3,
    pub matcap_factor: Vec3,
    pub outline_color_factor: Vec3,
    pub shading_shift_factor: f32,
    pub shading_toony_factor: f32,
    pub parametric_rim_fresnel_power_factor: f32,
    pub parametric_rim_lift_factor: f32,
    pub rim_lighting_mix_factor: f32,
    pub outline_width_factor: f32,
    pub outline_lighting_mix_factor: f32,
    pub uv_animation_scroll_x_speed_factor: f32,
    pub uv_animation_scroll_y_speed_factor: f32,
    pub uv_animation_rotation_speed_factor: f32,
    pub shading_shift_texture_scale: f32,
    pub texture_flags: u32,
    pub texture_uvs: u32,
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

impl From<&VrmcMaterialsMtoonSchema> for ToonParams {
    fn from(schema: &VrmcMaterialsMtoonSchema) -> Self {
        let mut params = Self::default();
        for (target, value) in [
            (&mut params.shade_color_factor, schema.shade_color_factor),
            (
                &mut params.parametric_rim_color_factor,
                schema.parametric_rim_color_factor,
            ),
            (&mut params.matcap_factor, schema.matcap_factor),
            (
                &mut params.outline_color_factor,
                schema.outline_color_factor,
            ),
        ] {
            if let Some(value) = value {
                *target = Vec3::from_array(value.map(|x| x as f32));
            }
        }
        for (target, value) in [
            (
                &mut params.shading_shift_factor,
                schema.shading_shift_factor,
            ),
            (
                &mut params.shading_toony_factor,
                schema.shading_toony_factor,
            ),
            (
                &mut params.parametric_rim_fresnel_power_factor,
                schema.parametric_rim_fresnel_power_factor,
            ),
            (
                &mut params.parametric_rim_lift_factor,
                schema.parametric_rim_lift_factor,
            ),
            (
                &mut params.rim_lighting_mix_factor,
                schema.rim_lighting_mix_factor,
            ),
            (
                &mut params.outline_width_factor,
                schema.outline_width_factor,
            ),
            (
                &mut params.outline_lighting_mix_factor,
                schema.outline_lighting_mix_factor,
            ),
            (
                &mut params.uv_animation_scroll_x_speed_factor,
                schema.uv_animation_scroll_x_speed_factor,
            ),
            (
                &mut params.uv_animation_scroll_y_speed_factor,
                schema.uv_animation_scroll_y_speed_factor,
            ),
            (
                &mut params.uv_animation_rotation_speed_factor,
                schema.uv_animation_rotation_speed_factor,
            ),
        ] {
            if let Some(value) = value {
                *target = value as f32;
            }
        }
        params
    }
}

/// Which outline pass a material needs. The pipeline keys on this, so it stays
/// a local type; `vrm-spec`'s [`OutlineWidthMode`] lacks `Eq` and `Default`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OutlineMode {
    None,
    WorldCoordinates,
    ScreenCoordinates,
}

#[derive(Clone, Deserialize)]
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

/// `KHR_texture_transform` parameters as authored.
///
/// Expressions interpolate the scale and offset of that transform, so keep the
/// source values instead of recovering them from a matrix that can be singular.
#[derive(Clone, Copy, Debug, Reflect)]
pub(crate) struct UvTransform {
    pub scale: Vec2,
    pub rotation: f32,
    pub offset: Vec2,
}

impl Default for UvTransform {
    fn default() -> Self {
        Self {
            scale: Vec2::ONE,
            rotation: 0.,
            offset: Vec2::ZERO,
        }
    }
}

impl UvTransform {
    pub fn from_json(info: &gltf::json::texture::Info) -> Self {
        let Some(transform) = texture_transform(info) else {
            return Self::default();
        };
        Self {
            scale: Vec2::from(transform.scale.0),
            rotation: -transform.rotation.0,
            offset: Vec2::from(transform.offset.0),
        }
    }

    pub fn from_gltf(transform: gltf::texture::TextureTransform<'_>) -> Self {
        Self {
            scale: Vec2::from(transform.scale()),
            rotation: -transform.rotation(),
            offset: Vec2::from(transform.offset()),
        }
    }

    pub fn affine(self) -> Affine2 {
        Affine2::from_scale_angle_translation(self.scale, self.rotation, self.offset)
    }
}

impl Toon {
    pub fn textures(&self) -> [Option<&gltf::json::texture::Info>; 6] {
        [
            self.schema.shade_multiply_texture.as_ref(),
            self.shading_shift_texture.as_ref().map(|s| &s.texture),
            self.schema.rim_multiply_texture.as_ref(),
            self.schema.matcap_texture.as_ref(),
            self.schema.uv_animation_mask_texture.as_ref(),
            self.schema.outline_width_multiply_texture.as_ref(),
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
    pub fn outline_mode(&self) -> OutlineMode {
        match self.schema.outline_width_mode {
            Some(OutlineWidthMode::WorldCoordinates) => OutlineMode::WorldCoordinates,
            Some(OutlineWidthMode::ScreenCoordinates) => OutlineMode::ScreenCoordinates,
            Some(OutlineWidthMode::None) | None => OutlineMode::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn toon_keeps_the_shading_shift_transform_and_spec_values() {
        let toon: Toon = serde_json::from_value(json!({
            "specVersion": "1.0",
            "outlineWidthMode": "screenCoordinates",
            "shadeColorFactor": [0.5, 0.25, 0.0],
            "shadingShiftTexture": {
                "index": 1,
                "scale": 0.5,
                "extensions": {"KHR_texture_transform": {
                    "offset": [0.1, 0.2],
                    "scale": [2.0, 3.0]
                }}
            }
        }))
        .unwrap();
        assert_eq!(toon.schema.spec_version, "1.0");
        assert_eq!(toon.outline_mode(), OutlineMode::ScreenCoordinates);
        assert_eq!(
            toon.shading_shift_texture.as_ref().unwrap().scale,
            Some(0.5)
        );
        assert_eq!(toon.textures()[1].unwrap().index.value(), 1);
        assert_eq!(
            ToonParams::from(&toon.schema).shade_color_factor,
            Vec3::new(0.5, 0.25, 0.0)
        );
        let uv = UvTransform::from_json(&toon.shading_shift_texture.as_ref().unwrap().texture);
        assert_eq!(uv.offset, Vec2::new(0.1, 0.2));
        assert_eq!(uv.scale, Vec2::new(2.0, 3.0));
    }
}
