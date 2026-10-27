// MToon lighting adapted from bevy_vrm1 (Copyright 2025 elm), MIT/Apache-2.0.
import bevy_pbr::render::{
    forward_io::{FragmentOutput, VertexOutput},
    mesh_view_bindings::{globals, lights, view},
    pbr_fragment::pbr_input_from_standard_material,
    pbr_functions::{alpha_discard, main_pass_post_lighting_processing},
};
import super::toon::{
    animated_uvs, matcap_sampler, matcap_texture, rim_sampler, rim_texture, shade_sampler,
    shade_texture, shift_sampler, shift_texture, texture_uv, toon,
};

@fragment
fn fragment(vertex: VertexOutput, @builtin(front_facing) front: bool) -> FragmentOutput {
    var input = vertex;
    var raw_uv0 = vec2<f32>(0.0);
    var raw_uv1 = vec2<f32>(0.0);
    @if(VERTEX_UVS_A) {
        raw_uv0 = input.uv;
    }
    @if(VERTEX_UVS_B) {
        raw_uv1 = input.uv_b;
    }
    @if(!VERTEX_UVS_A) {
        raw_uv0 = raw_uv1;
    }
    @if(!VERTEX_UVS_B) {
        raw_uv1 = raw_uv0;
    }
    let uvs = animated_uvs(raw_uv0, raw_uv1, globals.time);
    let uv0 = uvs.xy;
    let uv1 = uvs.zw;
    @if(VERTEX_UVS_A) {
        input.uv = uv0;
    }
    @if(VERTEX_UVS_B) {
        input.uv_b = uv1;
    }
    var material_front = front;
    @if(OUTLINE_PASS) {
        material_front = true;
    }
    var pbr = pbr_input_from_standard_material(input, material_front);
    pbr.material.base_color = alpha_discard(
        pbr.material.flags,
        pbr.material.alpha_cutoff,
        pbr.material.base_color,
    );
    let lit = pbr.material.base_color;
    var shade = toon.shade_color_factor;
    var shift = toon.shading_shift_factor;
    @if(MTOON_UVS) {
        let shade_uv = texture_uv(uv0, uv1, 0u);
        if (toon.texture_flags & 1u) != 0u {
            shade *= textureSampleBias(shade_texture, shade_sampler, shade_uv, view.mip_bias).rgb;
        }
        let shift_uv = texture_uv(uv0, uv1, 1u);
        if (toon.texture_flags & 2u) != 0u {
            shift += textureSampleBias(shift_texture, shift_sampler, shift_uv, view.mip_bias).r * toon.shading_shift_texture_scale;
        }
    }
    var direct = vec3(0.);
    var direct_lighting = vec3(0.);
    for (var i = 0u; i < lights.n_directional_lights; i++) {
        let light = lights.directional_lights[i];
        // Bevy supplies illuminance. Match the Lambert BRDF used by PBR and
        // three-vrm instead of treating lux as reflected radiance.
        let light_color = light.color.rgb / 3.141592653589793;
        let ndotl = dot(pbr.N, light.direction_to_light);
        let edge = max(1. - toon.shading_toony_factor, 0.00001);
        let shading = saturate((ndotl + shift + edge) / (2. * edge));
        direct += mix(shade, lit.rgb, shading) * light_color;
        direct_lighting += light_color * shading;
    }
    direct *= view.exposure;
    direct_lighting *= view.exposure;
    // No direction-dependent GI is supported, so `giEqualizationFactor` has
    // nothing to equalize and the uniform ambient term is the whole indirect.
    let ambient_lighting = view.exposure * lights.ambient_color.rgb;
    let indirect = ambient_lighting * lit.rgb * pbr.diffuse_occlusion;

    var rim = pow(saturate(1. - dot(pbr.N, pbr.V) + toon.parametric_rim_lift_factor), max(toon.parametric_rim_fresnel_power_factor, 0.0001)) * toon.parametric_rim_color_factor;
    if (toon.texture_flags & 8u) != 0u {
        let len_sq = dot(pbr.V.xz, pbr.V.xz);
        let world_view_x = select(normalize(vec3(pbr.V.z, 0.0, -pbr.V.x)), vec3(1.0, 0.0, 0.0), len_sq < 1e-6);
        let world_view_y = cross(pbr.V, world_view_x);
        // MToon's pseudocode uses +dot(worldViewY, N) in Unity's V-up space.
        // glTF/Bevy sample images with V-down, so negate Y to keep the matcap
        // upright; three-vrm's glTF-native implementation does the same.
        let matcap_uv = vec2(dot(world_view_x, pbr.N), -dot(world_view_y, pbr.N)) * 0.495 + 0.5;
        rim += textureSampleBias(matcap_texture, matcap_sampler, matcap_uv, view.mip_bias).rgb * toon.matcap_factor;
    }
    @if(MTOON_UVS) {
        let rim_uv = texture_uv(uv0, uv1, 2u);
        if (toon.texture_flags & 4u) != 0u {
            rim *= textureSampleBias(rim_texture, rim_sampler, rim_uv, view.mip_bias).rgb;
        }
    }
    // UniVRM MToon10's rim mix uses direct light color + indirect light (not the
    // lit surface color), so this intentionally stays a lighting-only factor.
    rim *= mix(vec3(1.), direct_lighting + ambient_lighting, toon.rim_lighting_mix_factor);
    let emission = pbr.material.emissive.rgb * mix(1., view.exposure, pbr.material.emissive.a);
    var color = vec4(direct + indirect + rim + emission, lit.a);
    @if(OUTLINE_PASS) {
        color = vec4(toon.outline_color_factor * mix(vec3(1.), color.rgb, toon.outline_lighting_mix_factor), lit.a);
    }
    var out: FragmentOutput;
    out.color = main_pass_post_lighting_processing(pbr, color);
    return out;
}
