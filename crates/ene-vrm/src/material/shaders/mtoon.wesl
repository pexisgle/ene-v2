// MToon fragment pass.
//
// Lighting follows the extension's lighting model: every directional light is
// evaluated with a shifted, feathered Lambert ramp that mixes
// `shadeColorFactor` and the lit color, then rim/matcap and ambient terms are
// added. Global illumination is limited to Bevy's uniform ambient term because
// MToon's direction-dependent GI is out of scope for this crate.
//
// Reference: `specification/VRMC_materials_mtoon-1.0/README.md` in the VRM
// specification.

import bevy_pbr::render::{
    forward_io::{FragmentOutput, VertexOutput},
    mesh_view_bindings::{globals, lights, view},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, main_pass_post_lighting_processing},
};
import super::mtoon_common::{
    SLOT_MATCAP, SLOT_RIM_MULTIPLY, SLOT_SHADE_MULTIPLY, SLOT_SHADING_SHIFT, animated_uvs,
    has_slot, matcap_sampler, matcap_texture, rim_multiply_sampler, rim_multiply_texture,
    shade_multiply_sampler, shade_multiply_texture, shading_shift_sampler, shading_shift_texture,
    slot_uv, toon,
};

/// Direct contribution split into the shaded surface color and the summed
/// light color, which the rim term mixes in independently of the surface.
struct Direct {
    color: vec3<f32>,
    lighting: vec3<f32>,
}

@fragment
fn fragment(vertex: VertexOutput, @builtin(front_facing) front: bool) -> FragmentOutput {
    var input = vertex;
    var uv0 = vec2<f32>(0.0);
    var uv1 = vec2<f32>(0.0);
    @if(VERTEX_UVS_A) {
        uv0 = input.uv;
    }
    @if(VERTEX_UVS_B) {
        uv1 = input.uv_b;
    }
    @if(!VERTEX_UVS_A) {
        uv0 = uv1;
    }
    @if(!VERTEX_UVS_B) {
        uv1 = uv0;
    }
    let animated = animated_uvs(uv0, uv1, globals.time);
    uv0 = animated.xy;
    uv1 = animated.zw;
    @if(VERTEX_UVS_A) {
        input.uv = uv0;
    }
    @if(VERTEX_UVS_B) {
        input.uv_b = uv1;
    }

    // Outlines always shade as front faces so the extruded shell keeps the
    // body's lighting instead of flipping with the winding.
    var front_facing = front;
    @if(OUTLINE_PASS) {
        front_facing = true;
    }
    var surface = pbr_input_from_standard_material(input, front_facing);
    surface.material.base_color = alpha_discard(
        surface.material.flags,
        surface.material.alpha_cutoff,
        surface.material.base_color,
    );
    let albedo = surface.material.base_color;

    var shade = toon.shade_color_factor;
    var shift = toon.shading_shift_factor;
    @if(MTOON_UVS) {
        if has_slot(SLOT_SHADE_MULTIPLY) {
            shade *= textureSampleBias(
                shade_multiply_texture,
                shade_multiply_sampler,
                slot_uv(uv0, uv1, 0u),
                view.mip_bias,
            ).rgb;
        }
        if has_slot(SLOT_SHADING_SHIFT) {
            shift += textureSampleBias(
                shading_shift_texture,
                shading_shift_sampler,
                slot_uv(uv0, uv1, 1u),
                view.mip_bias,
            ).r * toon.shading_shift_texture_scale;
        }
    }

    let direct = direct_lighting(surface.N, shade, albedo.rgb, shift);
    // No direction-dependent GI is supported, so `giEqualizationFactor` has
    // nothing to equalize and the uniform ambient term is the whole indirect.
    let ambient = view.exposure * lights.ambient_color.rgb;
    let indirect = ambient * albedo.rgb * surface.diffuse_occlusion;

    var rim = rim_term(surface.N, surface.V);
    if has_slot(SLOT_MATCAP) {
        rim += textureSampleBias(
            matcap_texture,
            matcap_sampler,
            matcap_uv(surface.N, surface.V),
            view.mip_bias,
        ).rgb * toon.matcap_factor;
    }
    @if(MTOON_UVS) {
        if has_slot(SLOT_RIM_MULTIPLY) {
            rim *= textureSampleBias(
                rim_multiply_texture,
                rim_multiply_sampler,
                slot_uv(uv0, uv1, 2u),
                view.mip_bias,
            ).rgb;
        }
    }
    // UniVRM's rim mix uses direct light plus indirect light, not the lit
    // surface color, so this is a lighting-only factor.
    rim *= mix(vec3<f32>(1.0), direct.lighting + ambient, toon.rim_lighting_mix_factor);

    let emission = surface.material.emissive.rgb
        * mix(1.0, view.exposure, surface.material.emissive.a);
    var color = vec4<f32>(direct.color + indirect + rim + emission, albedo.a);
    @if(OUTLINE_PASS) {
        color = vec4<f32>(
            toon.outline_color_factor
                * mix(vec3<f32>(1.0), color.rgb, toon.outline_lighting_mix_factor),
            albedo.a,
        );
    }

    var out: FragmentOutput;
    out.color = main_pass_post_lighting_processing(surface, color);
    return out;
}

/// Feathered Lambert ramp with `shadingToonyFactor` controlling the boundary
/// width and `shadingShiftFactor` moving it:
///
/// ```text
/// linearstep(-1 + toony, 1 - toony, dot(N, L) + shift)
/// ```
fn toon_ramp(ndotl: f32, shift: f32) -> f32 {
    let feather = max(1.0 - toon.shading_toony_factor, 0.00001);
    return saturate((ndotl + shift + feather) / (2.0 * feather));
}

fn direct_lighting(
    normal: vec3<f32>,
    shade: vec3<f32>,
    albedo: vec3<f32>,
    shift: f32,
) -> Direct {
    var result = Direct(vec3<f32>(0.0), vec3<f32>(0.0));
    for (var i = 0u; i < lights.n_directional_lights; i++) {
        let light = lights.directional_lights[i];
        // Bevy supplies illuminance. Match the Lambert BRDF used by PBR and
        // three-vrm instead of treating lux as reflected radiance.
        let radiance = light.color.rgb / 3.141592653589793;
        let ramp = toon_ramp(dot(normal, light.direction_to_light), shift);
        result.color += mix(shade, albedo, ramp) * radiance;
        result.lighting += radiance * ramp;
    }
    result.color *= view.exposure;
    result.lighting *= view.exposure;
    return result;
}

/// Fresnel rim around the surface silhouette.
fn rim_term(normal: vec3<f32>, view_dir: vec3<f32>) -> vec3<f32> {
    let fresnel = pow(
        saturate(1.0 - dot(normal, view_dir) + toon.parametric_rim_lift_factor),
        max(toon.parametric_rim_fresnel_power_factor, 0.0001),
    );
    return fresnel * toon.parametric_rim_color_factor;
}

/// View-space normal projected onto the matcap image.
fn matcap_uv(normal: vec3<f32>, view_dir: vec3<f32>) -> vec2<f32> {
    let horizontal_length_sq = dot(view_dir.xz, view_dir.xz);
    let view_x = select(
        normalize(vec3<f32>(view_dir.z, 0.0, -view_dir.x)),
        vec3<f32>(1.0, 0.0, 0.0),
        horizontal_length_sq < 1e-6,
    );
    let view_y = cross(view_dir, view_x);
    // Unity's matcap space is V-up; glTF samples images V-down, so negate Y.
    return vec2<f32>(dot(view_x, normal), -dot(view_y, normal)) * 0.495
        + vec2<f32>(0.5);
}
