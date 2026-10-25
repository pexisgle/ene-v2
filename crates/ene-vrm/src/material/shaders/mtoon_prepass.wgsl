#import "embedded://bevy_pbr/render/pbr_prepass.wgsl" as native_prepass
#import bevy_pbr::prepass_io::{FragmentOutput, VertexOutput}
#import bevy_render::globals::Globals
#import ene_vrm::toon::animated_uvs

// The prepass view layout binds Globals at group 0 binding 1; the main view
// layout exposes it at binding 11 through mesh_view_bindings.
@group(0) @binding(1) var<uniform> globals: Globals;

fn animated_output(input: VertexOutput) -> VertexOutput {
    var out = input;
    // The standard prepass vertex shader interpolates the raw UVs. Evaluate the
    // mask and scroll/rotation per fragment, exactly like mtoon.wgsl, instead
    // of interpolating a per-vertex UV transform.
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
    let animated = animated_uvs(raw_uv0, raw_uv1, globals.time);
#ifdef VERTEX_UVS_A
    out.uv = animated.xy;
#endif
#ifdef VERTEX_UVS_B
    out.uv_b = animated.zw;
#endif
    return out;
}

#ifdef PREPASS_FRAGMENT
@fragment
fn fragment(
    in: VertexOutput,
    @builtin(front_facing) is_front: bool,
) -> FragmentOutput {
    return native_prepass::fragment(animated_output(in), is_front);
}
#else
@fragment
fn fragment(in: VertexOutput) {
    native_prepass::fragment(animated_output(in));
}
#endif
