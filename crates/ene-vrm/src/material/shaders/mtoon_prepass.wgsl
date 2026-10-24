#import "embedded://bevy_pbr/prepass/prepass.wgsl" as native_prepass
#import bevy_pbr::prepass_io::{Vertex, VertexOutput}
#import bevy_render::globals::Globals
#import ene_vrm::toon::animated_uvs

// The prepass view layout binds Globals at group 0 binding 1; the main view
// layout exposes it at binding 11 through mesh_view_bindings.
@group(0) @binding(1) var<uniform> globals: Globals;

@vertex
fn vertex(input: Vertex) -> VertexOutput {
    // Read builtins at the entry point so Naga applies DX12 base vertex/instance offsets.
    var corrected = input;
    corrected.instance_index = input.instance_index;
#ifdef MORPH_TARGETS
    corrected.index = input.index;
#endif
    var out = native_prepass::vertex(corrected);
#ifdef MTOON_UVS
    // The standard prepass fragment samples the base color texture with `in.uv`
    // and the material's `uv_transform`; feed it the same animated UV as
    // mtoon.wgsl so alpha masking matches the main pass.
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
    // ponytail: the mask and rotation are evaluated per vertex here, matching
    // the outline path. A mask edge inside one triangle can differ from the
    // per-fragment main pass; move this to a custom prepass fragment shader if
    // that ever becomes visible.
    let uvs = animated_uvs(raw_uv0, raw_uv1, globals.time);
#ifdef VERTEX_UVS_A
    out.uv = uvs.xy;
#endif
#ifdef VERTEX_UVS_B
    out.uv_b = uvs.zw;
#endif
#endif
    return out;
}
