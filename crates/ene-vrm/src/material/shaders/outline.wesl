// Copied from bevy_pbr 0.20.0 `crates/bevy_pbr/src/render/mesh.wesl` (`vertex`),
// MIT OR Apache-2.0. Local changes: `morph_vertex` is imported instead of
// copied, and the outline extrusion is inserted before the clip-space
// conversion. Re-check this file against the upstream source when updating Bevy.
import bevy_pbr::render::{
    forward_io::{Vertex, VertexOutput, decompress_vertex},
    mesh_functions,
    mesh_view_bindings::{globals, view},
    skinning,
    view_transformations::position_world_to_clip,
};
import super::mtoon_common::{
    SLOT_OUTLINE_WIDTH, animated_uvs, has_slot, outline_width_sampler, outline_width_texture,
    slot_uv, toon,
};

// The standard mesh vertex entry point cannot be imported (WESL forbids calling
// another entry point), so this mirrors `bevy_pbr::render::mesh` and adds only
// the VRM outline extrusion. Its morph displacement is a plain function there,
// so it is imported instead of copied.
@if(MORPH_TARGETS)
import bevy_pbr::render::mesh::morph_vertex;

@vertex
fn vertex(vertex_no_morph: Vertex) -> VertexOutput {
    var out: VertexOutput;
    let uncompressed_vertex_no_morph = decompress_vertex(vertex_no_morph, vertex_no_morph.instance_index);
    @if(MORPH_TARGETS) {
        var vertex = morph_vertex(uncompressed_vertex_no_morph, vertex_no_morph.instance_index);
    }
    @if(!MORPH_TARGETS) {
        var vertex = uncompressed_vertex_no_morph;
    }

    let mesh_world_from_local = mesh_functions::get_world_from_local(vertex_no_morph.instance_index);

    @if(SKINNED) {
        // Use vertex_no_morph.instance_index instead of vertex.instance_index to work around a wgpu dx12 bug.
        // See https://github.com/gfx-rs/naga/issues/2416 .
        var world_from_local = skinning::skin_model(
            vertex.joint_indices,
            vertex.joint_weights,
            vertex_no_morph.instance_index
        );
    }
    @if(!SKINNED) {
        var world_from_local = mesh_world_from_local;
    }

    @if(VERTEX_NORMALS && SKINNED) {
        out.world_normal = skinning::skin_normals(world_from_local, vertex.normal);
    }
    @if(VERTEX_NORMALS && !SKINNED) {
        out.world_normal = mesh_functions::mesh_normal_local_to_world(
            vertex.normal,
            // Use vertex_no_morph.instance_index instead of vertex.instance_index to work around a wgpu dx12 bug.
            // See https://github.com/gfx-rs/naga/issues/2416
            vertex_no_morph.instance_index
        );
    }

    @if(VERTEX_POSITIONS) {
        out.world_position = mesh_functions::mesh_position_local_to_world(world_from_local, vec4<f32>(vertex.position, 1.0));
    }
    @if(VERTEX_POSITIONS) {
        out.position = position_world_to_clip(out.world_position.xyz);
    }

    @if(VERTEX_UVS_A) {
        out.uv = vertex.uv;
    }
    @if(VERTEX_UVS_B) {
        out.uv_b = vertex.uv_b;
    }

    @if(VERTEX_TANGENTS) {
        out.world_tangent = mesh_functions::mesh_tangent_local_to_world(
            world_from_local,
            vertex.tangent,
            // Use vertex_no_morph.instance_index instead of vertex.instance_index to work around a wgpu dx12 bug.
            // See https://github.com/gfx-rs/naga/issues/2416
            vertex_no_morph.instance_index
        );
    }

    @if(VERTEX_COLORS) {
        out.color = vertex.color;
    }

    @if(VERTEX_OUTPUT_INSTANCE_INDEX) {
        // Use vertex_no_morph.instance_index instead of vertex.instance_index to work around a wgpu dx12 bug.
        // See https://github.com/gfx-rs/naga/issues/2416
        out.instance_index = vertex_no_morph.instance_index;
    }

    @if(VISIBILITY_RANGE_DITHER) {
        out.visibility_range_dither = mesh_functions::get_visibility_range_dither_level(
            vertex_no_morph.instance_index, mesh_world_from_local[3]);
    }

    var width = toon.outline_width_factor;
    @if(MTOON_UVS) {
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
        let uvs = animated_uvs(raw_uv0, raw_uv1, globals.time);
        let uv0 = uvs.xy;
        let uv1 = uvs.zw;
        if has_slot(SLOT_OUTLINE_WIDTH) {
            width *= textureSampleLevel(
                outline_width_texture,
                outline_width_sampler,
                slot_uv(uv0, uv1, 5u),
                0.0,
            ).g;
        }
    }
    @if(SCREEN_SPACE_OUTLINE) {
        // Screen width is a fraction of viewport height, independent of distance.
        width *= 2. * abs(out.position.w) / abs(view.clip_from_view[1][1]);
    }
    @if(!SCREEN_SPACE_OUTLINE) {
        let world = mesh_functions::get_world_from_local(vertex_no_morph.instance_index);
        width *= max(length(world[0]), max(length(world[1]), length(world[2])));
    }
    out.world_position = vec4(out.world_position.xyz + normalize(out.world_normal) * width, 1.);
    out.position = position_world_to_clip(out.world_position.xyz);
    return out;
}
