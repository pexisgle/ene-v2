// Standard prepass fragment logic adapted from Bevy 0.20 (MIT OR Apache-2.0).
import bevy_pbr::prepass::io;
import bevy_pbr::render::pbr_functions::SampleBias;
import bevy_pbr::render::{
    mesh_bindings::mesh,
    mesh_view_bindings::view,
    pbr_bindings,
    pbr_functions,
    pbr_prepass_functions,
    pbr_types,
};
import bevy_render::bindless::{bindless_samplers_filtering, bindless_textures_2d};
import bevy_render::globals::Globals;
import super::toon::animated_uvs;

@if(BINDLESS)
import bevy_pbr::render::pbr_bindings::material_indices;

// The prepass view layout binds Globals at group 0 binding 1; the main view
// layout exposes it at binding 11 through mesh_view_bindings.
@group(0) @binding(1) var<uniform> globals: Globals;

fn animated_output(input: io::VertexOutput) -> io::VertexOutput {
    var out = input;
    // The standard prepass vertex shader interpolates the raw UVs. Evaluate the
    // mask and scroll/rotation per fragment, exactly like mtoon.wesl, instead
    // of interpolating a per-vertex UV transform.
    var raw_uv0 = vec2<f32>(0.0);
    var raw_uv1 = vec2<f32>(0.0);
    @if(VERTEX_UVS_A) {
        raw_uv0 = out.uv;
    }
    @if(VERTEX_UVS_B) {
        raw_uv1 = out.uv_b;
    }
    @if(!VERTEX_UVS_A) {
        raw_uv0 = raw_uv1;
    }
    @if(!VERTEX_UVS_B) {
        raw_uv1 = raw_uv0;
    }
    let animated = animated_uvs(raw_uv0, raw_uv1, globals.time);
    @if(VERTEX_UVS_A) {
        out.uv = animated.xy;
    }
    @if(VERTEX_UVS_B) {
        out.uv_b = animated.zw;
    }
    return out;
}

@if(PREPASS_FRAGMENT)
@fragment
fn fragment(
    input: io::VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> io::FragmentOutput {
    let in = animated_output(input);

    @if(BINDLESS)
    let slot = mesh[in.instance_index].material_and_lightmap_bind_group_slot & 0xffffu;
    @if(BINDLESS)
    let flags = pbr_bindings::material_array[material_indices[slot].material].flags;
    @if(BINDLESS)
    let uv_transform = pbr_bindings::material_array[material_indices[slot].material].uv_transform;
    @if(!BINDLESS)
    let flags = pbr_bindings::material.flags;
    @if(!BINDLESS)
    let uv_transform = pbr_bindings::material.uv_transform;

    @if(VISIBILITY_RANGE_DITHER) {
        pbr_functions::visibility_range_dither(in.position, in.visibility_range_dither);
    }
    pbr_prepass_functions::prepass_sample_color_and_alpha_discard(in);

    var out: io::FragmentOutput;
    @if(UNCLIPPED_DEPTH_ORTHO_EMULATION) {
        out.frag_depth = in.unclipped_depth;
    }
    @if(NORMAL_PREPASS) {
        if (flags & pbr_types::STANDARD_MATERIAL_FLAGS_UNLIT_BIT) == 0u {
            let double_sided = (flags & pbr_types::STANDARD_MATERIAL_FLAGS_DOUBLE_SIDED_BIT) != 0u;
            let mesh_flags = mesh[in.instance_index].flags;
            let is_logically_front = pbr_functions::winding_corrected_front_facing(mesh_flags, is_front);
            let world_normal = pbr_functions::prepare_world_normal(
                in.world_normal,
                double_sided,
                is_logically_front,
            );
            var normal = world_normal;
            @if(VERTEX_UVS) {
                @if(VERTEX_TANGENTS) {
                    @if(STANDARD_MATERIAL_NORMAL_MAP) {
                        @if(STANDARD_MATERIAL_NORMAL_MAP_UV_B)
                        let uv = (uv_transform * vec3(in.uv_b, 1.0)).xy;
                        @if(!STANDARD_MATERIAL_NORMAL_MAP_UV_B)
                        let uv = (uv_transform * vec3(in.uv, 1.0)).xy;
                        var bias: SampleBias;
                        bias.mip_bias = view.mip_bias;
                        @if(BINDLESS)
                        let texture = bindless_textures_2d[material_indices[slot].normal_map_texture];
                        @if(!BINDLESS)
                        let texture = pbr_bindings::normal_map_texture;
                        @if(BINDLESS)
                        let sampler_ = bindless_samplers_filtering[material_indices[slot].normal_map_sampler];
                        @if(!BINDLESS)
                        let sampler_ = pbr_bindings::normal_map_sampler;
                        let Nt = textureSampleBias(texture, sampler_, uv, bias.mip_bias).rgb;
                        let TBN = pbr_functions::calculate_tbn_mikktspace(normal, in.world_tangent);
                        normal = pbr_functions::apply_normal_mapping(
                            flags,
                            TBN,
                            double_sided,
                            is_logically_front,
                            Nt,
                        );
                    }
                }
            }
            out.normal = vec4(normal * 0.5 + vec3(0.5), 1.0);
        } else {
            out.normal = vec4(in.world_normal * 0.5 + vec3(0.5), 1.0);
        }
    }
    @if(MOTION_VECTOR_PREPASS) {
        out.motion_vector = pbr_prepass_functions::calculate_motion_vector(in.world_position, in.previous_world_position);
    }
    return out;
}

@if(!PREPASS_FRAGMENT)
@fragment
fn fragment(in: io::VertexOutput) {
    pbr_prepass_functions::prepass_sample_color_and_alpha_discard(animated_output(in));
}
