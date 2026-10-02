//! CPU triangle coverage candidate for comparison with the captured alpha mask.
//!
//! Call after animation, VRM constraints/springs and transform propagation.
//! `width` and `height` are **physical** target dimensions (as in `alpha_runs`);
//! returned runs use logical coordinates, with any covered physical sample
//! making its logical cell covered. Mesh assets must retain MAIN_WORLD data at
//! load time. This module never changes asset usage or performs GPU readback.
//! Texture alpha, material opacity, outlines, MSAA and depth are deliberately
//! absent: this is a geometric union, not an alternative MToon renderer.

use bevy::{
    asset::{AssetEvent, AssetId},
    camera::{RenderTarget, visibility::RenderLayers},
    ecs::message::MessageCursor,
    math::{DVec2, DVec4},
    mesh::{
        Indices, PrimitiveTopology, VertexAttributeValues,
        morph::{MeshMorphWeights, MorphWeights},
        skinning::{SkinnedMesh, SkinnedMeshInverseBindposes},
    },
    prelude::*,
    render::render_resource::Face,
};
use bevy_vrm1::prelude::MToonMaterial;
use std::collections::{HashMap, HashSet};

use crate::mask::Run;

struct SourceMesh {
    positions: Vec<Vec3>,
    triangles: Vec<[usize; 3]>,
    joints: Option<Vec<[u16; 4]>>,
    weights: Option<Vec<[f32; 4]>>,
    // Bevy 0.19 uses target-major MorphAttributes: target * vertex_count + vertex.
    morph_positions: Vec<Vec3>,
}

impl SourceMesh {
    fn read(mesh: &Mesh) -> Option<Self> {
        let VertexAttributeValues::Float32x3(positions) =
            mesh.try_attribute(Mesh::ATTRIBUTE_POSITION).ok()?
        else {
            return None;
        };
        if positions.is_empty() {
            return None;
        }
        let indices = mesh.try_indices_option().ok()?;
        let (indices, restart): (Vec<usize>, Option<usize>) = match indices {
            Some(Indices::U16(v)) => (
                v.iter().map(|&i| i as usize).collect(),
                Some(u16::MAX as usize),
            ),
            Some(Indices::U32(v)) => (
                v.iter().map(|&i| i as usize).collect(),
                Some(u32::MAX as usize),
            ),
            None => ((0..positions.len()).collect(), None),
        };
        let mut triangles = Vec::new();
        match mesh.primitive_topology() {
            PrimitiveTopology::TriangleList => {
                triangles.extend(indices.as_chunks::<3>().0.iter().copied());
            }
            PrimitiveTopology::TriangleStrip => {
                let mut strip = Vec::new();
                for index in indices {
                    if Some(index) == restart {
                        strip.clear();
                        continue;
                    }
                    strip.push(index);
                    let n = strip.len();
                    if n >= 3 {
                        let mut tri = [strip[n - 3], strip[n - 2], strip[n - 1]];
                        if n % 2 == 0 {
                            tri.swap(0, 1);
                        }
                        triangles.push(tri);
                    }
                }
            }
            _ => return None,
        }
        triangles.retain(|t| t.iter().all(|&i| i < positions.len()));
        let joints = match mesh.try_attribute(Mesh::ATTRIBUTE_JOINT_INDEX).ok() {
            Some(VertexAttributeValues::Uint16x4(v)) if v.len() == positions.len() => {
                Some(v.clone())
            }
            _ => None,
        };
        let weights = match mesh.try_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT).ok() {
            Some(VertexAttributeValues::Float32x4(v)) if v.len() == positions.len() => {
                Some(v.clone())
            }
            _ => None,
        };
        // The fallible accessors also handle assets already extracted to the GPU.
        let morph_positions = if mesh.try_has_morph_targets().ok()? {
            let targets = mesh.try_morph_targets().ok()?;
            if !targets.len().is_multiple_of(positions.len()) {
                return None;
            }
            targets.iter().map(|m| m.position).collect()
        } else {
            Vec::new()
        };
        Some(Self {
            positions: positions.iter().map(|&p| Vec3::from_array(p)).collect(),
            triangles,
            joints,
            weights,
            morph_positions,
        })
    }

    fn pose(
        &self,
        vertex: usize,
        morph: &[f32],
        model: Mat4,
        skin: Option<&[Mat4]>,
    ) -> Option<Vec3> {
        let mut position = self.positions[vertex];
        for (target, &weight) in morph.iter().enumerate() {
            if weight == 0.0 {
                continue;
            }
            if let Some(delta) = self
                .morph_positions
                .get(target * self.positions.len() + vertex)
            {
                position += *delta * weight;
            }
        }
        let position = if let Some(skin) = skin {
            let joints = self.joints.as_ref()?.get(vertex)?;
            let weights = self.weights.as_ref()?.get(vertex)?;
            let mut result = Vec3::ZERO;
            for (&joint, &weight) in joints.iter().zip(weights) {
                if weight != 0.0 {
                    // Skin matrices already map bind-space to world-space. Do
                    // not apply the mesh entity transform again or normalize
                    // weights (the shader does neither).
                    result += skin.get(joint as usize)?.transform_point3(position) * weight;
                }
            }
            result
        } else {
            model.transform_point3(position)
        };
        position.is_finite().then_some(position)
    }
}

#[derive(Resource, Default)]
struct GeometryCache {
    meshes: HashMap<AssetId<Mesh>, SourceMesh>,
    events: MessageCursor<AssetEvent<Mesh>>,
    // Frame scratch is reused; no full-resolution color/coverage buffer.
    vertices: Vec<DVec4>,
    rows: Vec<Vec<(u32, u32)>>,
}

struct View {
    clip_from_world: Mat4,
    viewport: URect,
    layers: RenderLayers,
    invert_culling: bool,
}

fn avatar_view(world: &mut World, width: u32, height: u32) -> Option<View> {
    let mut query = world.query_filtered::<(
        Entity,
        &Camera,
        &Projection,
        &GlobalTransform,
        Option<&RenderTarget>,
        Option<&RenderLayers>,
    ), With<Camera3d>>();
    // The avatar renderer has one offscreen image camera. Prefer its target,
    // then a matching size, then render order. Entity breaks ties deterministically.
    let (_, camera, projection, global, _, layers) = query
        .iter(world)
        .filter(|(_, c, _, _, target, _)| {
            c.is_active && !matches!(target, Some(RenderTarget::None { .. }))
        })
        .max_by_key(|(e, c, _, _, target, _)| {
            let image = target.and_then(RenderTarget::as_image);
            let size_matches = image
                .and_then(|h| world.get_resource::<Assets<Image>>()?.get(h))
                .is_some_and(|i| i.width() == width && i.height() == height);
            (
                image.is_some(),
                size_matches,
                c.order,
                std::cmp::Reverse(e.to_bits()),
            )
        })?;
    let clip_from_view = match camera.sub_camera_view.as_ref() {
        Some(sub) => projection.get_clip_from_view_for_sub(sub),
        None => projection.get_clip_from_view(),
    };
    let viewport = camera.viewport.as_ref().map_or(
        URect {
            min: UVec2::ZERO,
            max: UVec2::new(width, height),
        },
        |v| URect {
            min: v.physical_position,
            max: v.physical_position.saturating_add(v.physical_size),
        },
    );
    Some(View {
        clip_from_world: clip_from_view * global.to_matrix().inverse(),
        viewport,
        layers: layers.cloned().unwrap_or_default(),
        invert_culling: camera.invert_culling,
    })
}

fn morph_weights(world: &World, entity: Entity) -> &[f32] {
    match world.get::<MeshMorphWeights>(entity) {
        Some(MeshMorphWeights::Value { weights }) => weights,
        Some(MeshMorphWeights::Reference(source)) => world
            .get::<MorphWeights>(*source)
            .map_or(&[], MorphWeights::weights),
        None => &[],
    }
}

fn skin_matrices(world: &World, skin: &SkinnedMesh) -> Option<Vec<Mat4>> {
    let bind = world
        .get_resource::<Assets<SkinnedMeshInverseBindposes>>()?
        .get(&skin.inverse_bindposes)?;
    skin.joints
        .iter()
        .enumerate()
        .map(|(i, &joint)| Some(world.get::<GlobalTransform>(joint)?.to_matrix() * *bind.get(i)?))
        .collect()
}

fn cull_mode(world: &World, entity: Entity) -> Option<Face> {
    if let Some(handle) = world.get::<MeshMaterial3d<MToonMaterial>>(entity) {
        return world
            .get_resource::<Assets<MToonMaterial>>()?
            .get(&handle.0)?
            .cull_mode;
    }
    if let Some(handle) = world.get::<MeshMaterial3d<StandardMaterial>>(entity) {
        return world
            .get_resource::<Assets<StandardMaterial>>()?
            .get(&handle.0)?
            .cull_mode;
    }
    // Unknown materials may be double-sided; keep both sides.
    None
}

/// Project currently posed Mesh3d triangles into logical input-region runs.
/// Dimensions are physical pixels, `scale` is the integer Wayland buffer scale.
/// Missing cameras/assets or invalid dimensions yield an empty mask. Missing
/// skin data skips that mesh rather than silently drawing its bind pose.
pub fn projected_runs(world: &mut World, width: u32, height: u32, scale: u32) -> Vec<Run> {
    if width == 0
        || height == 0
        || scale == 0
        || width > i32::MAX as u32
        || height > i32::MAX as u32
    {
        return Vec::new();
    }
    let Some(view) = avatar_view(world, width, height) else {
        return Vec::new();
    };
    if !view.clip_from_world.is_finite() || view.viewport.is_empty() {
        return Vec::new();
    }
    let instances: Vec<_> = world
        .query::<(
            Entity,
            &Mesh3d,
            &GlobalTransform,
            Option<&InheritedVisibility>,
            Option<&Visibility>,
            Option<&RenderLayers>,
        )>()
        .iter(world)
        .filter(|(_, _, _, inherited, visibility, layers)| {
            inherited.is_none_or(|v| v.get())
                && !matches!(visibility, Some(Visibility::Hidden))
                && view.layers.intersects(layers.unwrap_or_default())
        })
        .map(|(entity, mesh, global, _, _, _)| (entity, mesh.0.id(), global.to_matrix()))
        .collect();
    let mut cache = world.remove_resource::<GeometryCache>().unwrap_or_default();
    if let Some(events) = world.get_resource::<Messages<AssetEvent<Mesh>>>() {
        if cache.events.missed_messages(events) > 0 {
            cache.meshes.clear();
        }
        for event in cache.events.read(events) {
            match event {
                AssetEvent::Added { id }
                | AssetEvent::Modified { id }
                | AssetEvent::Removed { id }
                | AssetEvent::Unused { id } => {
                    cache.meshes.remove(id);
                }
                AssetEvent::LoadedWithDependencies { .. } => {}
            }
        }
    } else {
        // In bare Worlds without AssetPlugin there is no invalidation stream.
        cache.meshes.clear();
    }
    let live: HashSet<_> = instances.iter().map(|(_, id, _)| *id).collect();
    cache.meshes.retain(|id, _| live.contains(id));
    cache
        .rows
        .resize_with(height.div_ceil(scale) as usize, Vec::new);
    for row in &mut cache.rows {
        row.clear();
    }
    for (entity, id, model) in instances {
        if let std::collections::hash_map::Entry::Vacant(entry) = cache.meshes.entry(id)
            && let Some(source) = world
                .get_resource::<Assets<Mesh>>()
                .and_then(|a| a.get(id))
                .and_then(SourceMesh::read)
        {
            entry.insert(source);
        }
        let Some(source) = cache.meshes.get(&id) else {
            continue;
        };
        let skin = if let Some(skin) = world.get::<SkinnedMesh>(entity) {
            let Some(matrices) = skin_matrices(world, skin) else {
                continue;
            };
            Some(matrices)
        } else {
            None
        };
        let morph = morph_weights(world, entity);
        cache.vertices.clear();
        cache.vertices.extend((0..source.positions.len()).map(|i| {
            source
                .pose(i, morph, model, skin.as_deref())
                .map(|p| (view.clip_from_world * p.extend(1.0)).as_dvec4())
                .unwrap_or(DVec4::NAN)
        }));
        let cull = cull_mode(world, entity);
        for triangle in &source.triangles {
            raster_clipped(
                triangle.map(|i| cache.vertices[i]),
                &view,
                cull,
                width,
                height,
                scale,
                &mut cache.rows,
            );
        }
    }
    let mut runs = Vec::new();
    for (y, row) in cache.rows.iter_mut().enumerate() {
        row.sort_unstable();
        let mut merged: Option<(u32, u32)> = None;
        for &(start, end) in row.iter() {
            match merged {
                Some((left, right)) if start <= right => merged = Some((left, right.max(end))),
                previous => {
                    if let Some((left, right)) = previous {
                        runs.push(Run {
                            x: left as i32,
                            y: y as i32,
                            width: (right - left) as i32,
                        });
                    }
                    merged = Some((start, end));
                }
            }
        }
        if let Some((left, right)) = merged {
            runs.push(Run {
                x: left as i32,
                y: y as i32,
                width: (right - left) as i32,
            });
        }
    }
    world.insert_resource(cache);
    runs
}

// WebGPU clip volume: -w <= x,y <= w, 0 <= z <= w (including
// Bevy's reverse-Z projection). Clip BEFORE perspective division. f64 and a
// fixed-size polygon avoid overflow and allocation for eye-plane crossings.
fn clip_triangle(triangle: [DVec4; 3]) -> ([DVec4; 12], usize) {
    let mut polygon = [DVec4::ZERO; 12];
    if triangle.iter().any(|p| !p.is_finite()) {
        return (polygon, 0);
    }
    let magnitude = triangle
        .iter()
        .map(|p| p.abs().max_element())
        .fold(0.0, f64::max);
    if magnitude == 0.0 {
        return (polygon, 0);
    }
    for (p, v) in polygon.iter_mut().zip(triangle) {
        *p = v / magnitude;
    }
    let mut count = 3;
    for plane in 0..7 {
        let distance = |p: DVec4| match plane {
            0 => p.w + p.x,
            1 => p.w - p.x,
            2 => p.w + p.y,
            3 => p.w - p.y,
            4 => p.z,
            5 => p.w - p.z,
            // The clip-volume apex has w=0 and cannot be divided. This tiny
            // relative guard only removes the singularity, not a near plane.
            _ => p.w - 1e-12,
        };
        let mut output = [DVec4::ZERO; 12];
        let mut n = 0;
        if count == 0 {
            break;
        }
        let mut previous = polygon[count - 1];
        let mut previous_d = distance(previous);
        for &current in &polygon[..count] {
            let current_d = distance(current);
            if (previous_d >= 0.0) != (current_d >= 0.0) {
                output[n] = previous.lerp(current, previous_d / (previous_d - current_d));
                n += 1;
            }
            if current_d >= 0.0 {
                output[n] = current;
                n += 1;
            }
            previous = current;
            previous_d = current_d;
        }
        polygon = output;
        count = n;
    }
    (polygon, count)
}

fn raster_clipped(
    triangle: [DVec4; 3],
    view: &View,
    cull: Option<Face>,
    width: u32,
    height: u32,
    scale: u32,
    rows: &mut [Vec<(u32, u32)>],
) {
    let (polygon, count) = clip_triangle(triangle);
    if count < 3 {
        return;
    }
    let screen = |p: DVec4| {
        let ndc = p.truncate() / p.w;
        let size = view.viewport.size().as_dvec2();
        view.viewport.min.as_dvec2() + DVec2::new(ndc.x + 1.0, 1.0 - ndc.y) * 0.5 * size
    };
    let first = screen(polygon[0]);
    for i in 1..count - 1 {
        let tri = [first, screen(polygon[i]), screen(polygon[i + 1])];
        let area = (tri[1] - tri[0]).perp_dot(tri[2] - tri[0]);
        if !area.is_finite() || area == 0.0 {
            continue;
        }
        // CCW in NDC is clockwise (negative signed area) in screen coordinates.
        let front = (area < 0.0) ^ view.invert_culling;
        if matches!(cull, Some(Face::Back)) && !front || matches!(cull, Some(Face::Front)) && front
        {
            continue;
        }
        raster_triangle(tri, width, height, scale, rows);
    }
}

fn raster_triangle(
    tri: [DVec2; 3],
    width: u32,
    height: u32,
    scale: u32,
    rows: &mut [Vec<(u32, u32)>],
) {
    let min_y = tri.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let max_y = tri.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
    let first_y = (min_y - 0.5).ceil().clamp(0.0, height as f64) as u32;
    let end_y = (max_y - 0.5).ceil().clamp(0.0, height as f64) as u32;
    for y in first_y..end_y {
        let sample_y = y as f64 + 0.5;
        let mut left = f64::INFINITY;
        let mut right = f64::NEG_INFINITY;
        for i in 0..3 {
            let mut a = tri[i];
            let mut b = tri[(i + 1) % 3];
            if a.y > b.y {
                std::mem::swap(&mut a, &mut b);
            }
            if sample_y >= a.y && sample_y < b.y {
                let x = a.x + (sample_y - a.y) / (b.y - a.y) * (b.x - a.x);
                left = left.min(x);
                right = right.max(x);
            }
        }
        if left >= right {
            continue;
        }
        // Top/left edges inclusive; bottom/right exclusive at pixel centers.
        let start = (left - 0.5).ceil().clamp(0.0, width as f64) as u32;
        let end = (right - 0.5).ceil().clamp(0.0, width as f64) as u32;
        if start < end {
            rows[(y / scale) as usize].push((start / scale, (end - 1) / scale + 1));
        }
    }
}
