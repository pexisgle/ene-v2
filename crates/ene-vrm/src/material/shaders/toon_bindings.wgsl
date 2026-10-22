#define_import_path ene_vrm::toon

struct Toon {
    shade_color_factor: vec3<f32>,
    parametric_rim_color_factor: vec3<f32>,
    matcap_factor: vec3<f32>,
    outline_color_factor: vec3<f32>,
    shading_shift_factor: f32,
    shading_toony_factor: f32,
    gi_equalization_factor: f32,
    parametric_rim_fresnel_power_factor: f32,
    parametric_rim_lift_factor: f32,
    rim_lighting_mix_factor: f32,
    outline_width_factor: f32,
    outline_lighting_mix_factor: f32,
    uv_animation_scroll_x_speed_factor: f32,
    uv_animation_scroll_y_speed_factor: f32,
    uv_animation_rotation_speed_factor: f32,
    shading_shift_texture_scale: f32,
    texture_flags: u32,
}
@group(#{MATERIAL_BIND_GROUP}) @binding(100) var<uniform> toon: Toon;
@group(#{MATERIAL_BIND_GROUP}) @binding(101) var shade_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(102) var shade_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(103) var shift_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(104) var shift_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(105) var rim_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(106) var rim_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(107) var matcap_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(108) var matcap_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(109) var uv_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(110) var uv_sampler: sampler;
@group(#{MATERIAL_BIND_GROUP}) @binding(111) var width_texture: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(112) var width_sampler: sampler;

#import bevy_pbr::{mesh_view_bindings::globals, pbr_bindings::material}
fn linear_to_srgb_f32(c: f32) -> f32 {
    if c <= 0.0031308 {
        return c * 12.92;
    } else {
        return 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    }
}
fn animated_uv(uv: vec2<f32>) -> vec2<f32> {
    var time = globals.time;
    if (toon.texture_flags & 16u) != 0u { time *= linear_to_srgb_f32(textureSampleLevel(uv_texture, uv_sampler, (material.uv_transform * vec3(uv, 1.0)).xy, 0.).b); }
    let angle = time * toon.uv_animation_rotation_speed_factor;
    let c = cos(angle);
    let s = sin(angle);
    return mat2x2(c, s, -s, c) * (uv - 0.5) + 0.5 + time * vec2(toon.uv_animation_scroll_x_speed_factor, toon.uv_animation_scroll_y_speed_factor);
}
