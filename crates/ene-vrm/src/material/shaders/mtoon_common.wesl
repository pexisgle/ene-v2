// Shared `VRMC_materials_mtoon` parameters, texture bindings, and the UV
// animation used by every MToon pass.
//
// The uniform layout mirrors `ToonParams` in `material/mod.rs`; binding numbers
// are part of `MToonExtension`'s `AsBindGroup` layout and must not change.
// Texture slots follow the extension order:
//
// 0 shadeMultiply, 1 shadingShift, 2 rimMultiply, 3 matcap, 4 uvAnimationMask,
// 5 outlineWidthMultiply.
//
// Reference: `specification/VRMC_materials_mtoon-1.0/README.md` in the VRM
// specification.

struct MToon {
    shade_color_factor: vec3<f32>,
    parametric_rim_color_factor: vec3<f32>,
    matcap_factor: vec3<f32>,
    outline_color_factor: vec3<f32>,
    shading_shift_factor: f32,
    shading_toony_factor: f32,
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
    texture_uvs: u32,
    uv_transforms: array<mat3x3<f32>, 6>,
}

@group(constants::MATERIAL_BIND_GROUP) @binding(100) var<uniform> toon: MToon;
@group(constants::MATERIAL_BIND_GROUP) @binding(101) var shade_multiply_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(102) var shade_multiply_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(103) var shading_shift_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(104) var shading_shift_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(105) var rim_multiply_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(106) var rim_multiply_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(107) var matcap_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(108) var matcap_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(109) var uv_animation_mask_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(110) var uv_animation_mask_sampler: sampler;
@group(constants::MATERIAL_BIND_GROUP) @binding(111) var outline_width_texture: texture_2d<f32>;
@group(constants::MATERIAL_BIND_GROUP) @binding(112) var outline_width_sampler: sampler;

/// `texture_flags` bit per texture slot. A set bit means the slot is bound.
const SLOT_SHADE_MULTIPLY: u32 = 1u;
const SLOT_SHADING_SHIFT: u32 = 2u;
const SLOT_RIM_MULTIPLY: u32 = 4u;
const SLOT_MATCAP: u32 = 8u;
const SLOT_UV_ANIMATION_MASK: u32 = 16u;
const SLOT_OUTLINE_WIDTH: u32 = 32u;

fn has_slot(flag: u32) -> bool {
    return (toon.texture_flags & flag) != 0u;
}

/// Samples a semantic texture with the TEXCOORD and `KHR_texture_transform`
/// assigned to its slot.
fn slot_uv(uv0: vec2<f32>, uv1: vec2<f32>, slot: u32) -> vec2<f32> {
    let uv = select(uv0, uv1, ((toon.texture_uvs >> slot) & 1u) != 0u);
    return (toon.uv_transforms[slot] * vec3<f32>(uv, 1.0)).xy;
}

/// The UV animation mask stores its weight in the blue channel; unbound masks
/// animate fully.
fn mask_weight(mask_uv: vec2<f32>) -> f32 {
    if !has_slot(SLOT_UV_ANIMATION_MASK) {
        return 1.0;
    }
    return textureSampleLevel(uv_animation_mask_texture, uv_animation_mask_sampler, mask_uv, 0.0).b;
}

/// Applies the extension's per-frame scroll and rotation around the texture
/// center, scaled by the mask weight.
fn animate_uv(uv: vec2<f32>, mask: f32, time: f32) -> vec2<f32> {
    let scaled_time = time * mask;
    let angle = scaled_time * toon.uv_animation_rotation_speed_factor;
    let c = cos(angle);
    let s = sin(angle);
    // The spec composes [[cos, sin], [-sin, cos]] * (uv - 0.5). WGSL matrices
    // are column-major, so the columns are (c, -s) and (s, c).
    let rotated = mat2x2(c, -s, s, c) * (uv - 0.5);
    let scrolled = scaled_time
        * vec2<f32>(
            toon.uv_animation_scroll_x_speed_factor,
            toon.uv_animation_scroll_y_speed_factor,
        );
    return rotated + vec2<f32>(0.5) + scrolled;
}

/// Animated UV0 and UV1, packed so callers need no struct and the two stay in
/// sync with the mask sampled once per fragment.
fn animated_uvs(raw_uv0: vec2<f32>, raw_uv1: vec2<f32>, time: f32) -> vec4<f32> {
    let mask = mask_weight(slot_uv(raw_uv0, raw_uv1, 4u));
    return vec4<f32>(animate_uv(raw_uv0, mask, time), animate_uv(raw_uv1, mask, time));
}
