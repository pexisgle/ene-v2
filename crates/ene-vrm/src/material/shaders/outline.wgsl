#import "embedded://bevy_pbr/render/mesh.wgsl" as native_mesh
#import bevy_pbr::{forward_io::{Vertex, VertexOutput}, mesh_functions, mesh_view_bindings::view, view_transformations::position_world_to_clip}
#import ene_vrm::toon::{toon, animated_uv, uv_animation_mask, texture_uv, width_texture, width_sampler}

@vertex
fn vertex(input: Vertex) -> VertexOutput {
    // Read builtins at the entry point so Naga applies DX12 base vertex/instance offsets.
    var corrected = input;
    corrected.instance_index = input.instance_index;
#ifdef MORPH_TARGETS
    corrected.index = input.index;
#endif
    var out = native_mesh::vertex(corrected);
    var width = toon.outline_width_factor;
#ifdef MTOON_UVS
#ifdef VERTEX_UVS_A
    var raw_uv0 = out.uv;
#else
    var raw_uv0 = vec2<f32>(0.0);
#endif
#ifdef VERTEX_UVS_B
    var raw_uv1 = out.uv_b;
#else
    var raw_uv1 = vec2<f32>(0.0);
#endif
#ifndef VERTEX_UVS_A
    raw_uv0 = raw_uv1;
#endif
#ifndef VERTEX_UVS_B
    raw_uv1 = raw_uv0;
#endif
    let mask = uv_animation_mask(texture_uv(raw_uv0, raw_uv1, 4u));
    let uv0 = animated_uv(raw_uv0, mask);
    let uv1 = animated_uv(raw_uv1, mask);
    if (toon.texture_flags & 32u) != 0u {
        width *= textureSampleLevel(width_texture, width_sampler, texture_uv(uv0, uv1, 5u), 0.).g;
    }
#endif
#ifdef SCREEN_SPACE_OUTLINE
    // Screen width is a fraction of viewport height, independent of distance.
    width *= 2. * abs(out.position.w) / abs(view.clip_from_view[1][1]);
#else
    let world = mesh_functions::get_world_from_local(input.instance_index);
    width *= max(length(world[0]), max(length(world[1]), length(world[2])));
#endif
    out.world_position = vec4(out.world_position.xyz + normalize(out.world_normal) * width, 1.);
    out.position = position_world_to_clip(out.world_position.xyz);
    return out;
}
