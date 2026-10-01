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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{asset::RenderAssetUsages, mesh::morph::MorphAttributes};

    fn test_world() -> World {
        let mut world = World::new();
        world.init_resource::<Assets<Mesh>>();
        world.init_resource::<Assets<SkinnedMeshInverseBindposes>>();
        world.init_resource::<Messages<AssetEvent<Mesh>>>();
        world.spawn((
            Camera3d::default(),
            Camera::default(),
            Projection::Orthographic(OrthographicProjection {
                area: Rect::from_corners(Vec2::splat(-1.0), Vec2::ONE),
                near: 0.0,
                far: 10.0,
                ..OrthographicProjection::default_3d()
            }),
            GlobalTransform::IDENTITY,
        ));
        world
    }

    fn mesh(positions: Vec<[f32; 3]>) -> Mesh {
        Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
    }

    fn spawn_mesh(world: &mut World, mesh: Mesh) -> (Entity, Handle<Mesh>) {
        let handle = world.resource_mut::<Assets<Mesh>>().add(mesh);
        let entity = world
            .spawn((
                Mesh3d(handle.clone()),
                GlobalTransform::IDENTITY,
                InheritedVisibility::VISIBLE,
            ))
            .id();
        (entity, handle)
    }

    // Rectangles specified in physical screen coordinates of an 8x8 target.
    fn rectangle(left: f32, top: f32, right: f32, bottom: f32) -> Mesh {
        let p = |x: f32, y: f32| [x / 4.0 - 1.0, 1.0 - y / 4.0, -1.0];
        mesh(vec![
            p(left, top),
            p(left, bottom),
            p(right, top),
            p(right, bottom),
        ])
        .with_inserted_indices(Indices::U16(vec![0, 1, 2, 2, 1, 3]))
    }

    #[test]
    fn geometric_parts_keep_holes_and_match_physical_alpha_reduction() {
        let mut world = test_world();
        spawn_mesh(&mut world, rectangle(0.0, 0.0, 2.0, 3.0));
        spawn_mesh(&mut world, rectangle(4.0, 0.0, 8.0, 3.0));
        spawn_mesh(&mut world, rectangle(1.0, 2.0, 2.0, 4.0)); // Overlap/adjacent rows.
        let mut rgba = vec![0; 8 * 8 * 4];
        for y in 0..4 {
            for x in 0..8 {
                if (y < 3 && !(2..4).contains(&x)) || (y == 3 && x == 1) {
                    rgba[(y * 8 + x) * 4 + 3] = 255;
                }
            }
        }
        for scale in [1, 2, 3, 5, 9] {
            assert_eq!(
                projected_runs(&mut world, 8, 8, scale),
                crate::mask::alpha_runs(&rgba, 8, 8, scale)
            );
        }
        let runs = projected_runs(&mut world, 8, 8, 1);
        assert!(
            !crate::mask::contains(&runs, (3.5, 1.5)),
            "gap between geometric parts"
        );
        assert!(crate::mask::contains(&runs, (4.5, 1.5)));
    }

    #[test]
    fn scanline_coverage_matches_top_left_edge_reference() {
        // A separately written per-sample edge test validates the scanline
        // interval algorithm, including edges passing through sample centers.
        let cases = [
            [(0.0, 0.0), (8.0, 0.0), (0.0, 8.0)],
            [(0.5, 0.5), (7.5, 2.5), (2.5, 7.5)],
            [(2.0, 1.5), (6.0, 1.5), (4.0, 6.5)],
            [(1.2, 2.3), (6.6, 7.1), (4.3, 0.1)],
            [(-3.0, -2.0), (12.0, 3.0), (2.0, 12.0)],
        ];
        for points in cases {
            for reverse in [false, true] {
                let mut tri = points.map(|(x, y)| DVec2::new(x, y));
                if reverse {
                    tri.swap(0, 1);
                }
                let mut rows = vec![Vec::new(); 8];
                raster_triangle(tri, 8, 8, 1, &mut rows);
                if (tri[1] - tri[0]).perp_dot(tri[2] - tri[0]) < 0.0 {
                    tri.swap(0, 1);
                }
                for (y, row) in rows.iter().enumerate() {
                    for x in 0..8 {
                        let p = DVec2::new(x as f64 + 0.5, y as f64 + 0.5);
                        let expected = (0..3).all(|i| {
                            let a = tri[i];
                            let b = tri[(i + 1) % 3];
                            let edge = b - a;
                            let cross = edge.perp_dot(p - a);
                            cross > 0.0
                                || (cross == 0.0
                                    && (edge.y < 0.0 || (edge.y == 0.0 && edge.x > 0.0)))
                        });
                        let actual = row.iter().any(|&(a, b)| x as u32 >= a && (x as u32) < b);
                        assert_eq!(actual, expected, "triangle {tri:?}, sample ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn all_six_homogeneous_planes_are_clipped_without_screen_explosions() {
        let inside = DVec4::new(0.0, 0.2, 0.5, 1.0);
        for outside in [
            DVec4::new(-3.0, -0.5, 0.5, 1.0),
            DVec4::new(3.0, -0.5, 0.5, 1.0),
            DVec4::new(-0.5, -3.0, 0.5, 1.0),
            DVec4::new(-0.5, 3.0, 0.5, 1.0),
            DVec4::new(-0.5, -0.5, -2.0, 1.0),
            DVec4::new(-0.5, -0.5, 3.0, 1.0),
            DVec4::new(-0.5, -0.5, -0.5, -1.0),
            DVec4::ZERO,
        ] {
            for multiplier in [1e-100, 1.0, 1e100] {
                let tri =
                    [inside, outside, DVec4::new(0.7, -0.2, 0.5, 1.0)].map(|v| v * multiplier);
                let (polygon, count) = clip_triangle(tri);
                assert!((3..=10).contains(&count));
                for p in &polygon[..count] {
                    assert!(p.is_finite() && p.w > 0.0);
                    let ndc = p.truncate() / p.w;
                    assert!(ndc.x.abs() <= 1.0 + 1e-9 && ndc.y.abs() <= 1.0 + 1e-9);
                    assert!((-1e-9..=1.0 + 1e-9).contains(&ndc.z));
                }
            }
        }
        for tri in [
            [DVec4::new(3.0, 0.0, 0.5, 1.0); 3],
            [DVec4::new(0.0, 0.0, 0.5, -1.0); 3],
            [DVec4::ZERO; 3],
            [DVec4::NAN; 3],
        ] {
            assert_eq!(clip_triangle(tri).1, 0);
        }
    }

    #[test]
    fn perspective_near_plane_and_behind_camera_are_handled() {
        let mut world = test_world();
        let camera = world
            .query_filtered::<Entity, With<Camera3d>>()
            .single(&world)
            .unwrap();
        world
            .entity_mut(camera)
            .insert(Projection::Perspective(PerspectiveProjection {
                fov: std::f32::consts::FRAC_PI_2,
                aspect_ratio: 1.0,
                near: 0.1,
                ..default()
            }));
        let (entity, _) = spawn_mesh(
            &mut world,
            mesh(vec![[-0.5, -0.5, -1.0], [0.5, -0.5, -1.0], [0.0, 0.5, 0.5]]),
        );
        let runs = projected_runs(&mut world, 8, 8, 1);
        assert!(!runs.is_empty());
        assert!(
            runs.iter()
                .all(|r| r.x >= 0 && r.y >= 0 && r.x + r.width <= 8 && r.y < 8)
        );
        world
            .entity_mut(entity)
            .insert(GlobalTransform::from(Transform::from_xyz(0.0, 0.0, 3.0)));
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
    }

    #[test]
    fn skin_uses_current_joint_and_inverse_bind_without_double_model_transform() {
        let mut world = test_world();
        let source_mesh = rectangle(0.0, 0.0, 2.0, 2.0)
            .with_inserted_attribute(
                Mesh::ATTRIBUTE_JOINT_INDEX,
                VertexAttributeValues::Uint16x4(vec![[0, 1, 0, 0]; 4]),
            )
            .with_inserted_attribute(
                Mesh::ATTRIBUTE_JOINT_WEIGHT,
                vec![[0.25, 0.75, 0.0, 0.0]; 4],
            );
        let (entity, _) = spawn_mesh(&mut world, source_mesh);
        let joint_a = world
            .spawn(GlobalTransform::from(Transform::from_xyz(1.0, 0.0, 0.0)))
            .id();
        let joint_b = world
            .spawn(GlobalTransform::from(Transform::from_xyz(1.0, 0.0, 0.0)))
            .id();
        let bind = world
            .resource_mut::<Assets<SkinnedMeshInverseBindposes>>()
            .add(SkinnedMeshInverseBindposes::from(
                vec![Mat4::from_translation(Vec3::NEG_X); 2],
            ));
        world.entity_mut(entity).insert((
            SkinnedMesh {
                inverse_bindposes: bind,
                joints: vec![joint_a, joint_b],
            },
            GlobalTransform::from(Transform::from_xyz(100.0, 100.0, 100.0)),
        ));
        let rest = projected_runs(&mut world, 8, 8, 1);
        assert_eq!(
            rest,
            vec![
                Run {
                    x: 0,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 0,
                    y: 1,
                    width: 2
                }
            ]
        );
        // Both translations weighted together move by exactly 0.5 world units (2 px).
        world
            .entity_mut(joint_a)
            .insert(GlobalTransform::from(Transform::from_xyz(0.5, 0.0, 0.0)));
        world
            .entity_mut(joint_b)
            .insert(GlobalTransform::from(Transform::from_xyz(
                11.0 / 6.0,
                0.0,
                0.0,
            )));
        let moved = projected_runs(&mut world, 8, 8, 1);
        assert_eq!(
            moved,
            rest.iter()
                .map(|r| Run { x: r.x + 2, ..*r })
                .collect::<Vec<_>>()
        );
        world.despawn(joint_a);
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
    }

    #[test]
    fn morph_target_major_layout_value_and_reference_weights_are_live() {
        let mut world = test_world();
        let mut shape = rectangle(0.0, 0.0, 2.0, 2.0);
        let mut deltas = vec![MorphAttributes::new(Vec3::X, Vec3::ZERO, Vec3::ZERO); 4];
        deltas.extend(vec![
            MorphAttributes::new(Vec3::Y, Vec3::ZERO, Vec3::ZERO);
            4
        ]);
        shape.set_morph_targets(deltas);
        let (entity, _) = spawn_mesh(&mut world, shape);
        world.entity_mut(entity).insert(MeshMorphWeights::Value {
            weights: vec![0.5, -0.5],
        });
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 2,
                    y: 2,
                    width: 2
                },
                Run {
                    x: 2,
                    y: 3,
                    width: 2
                }
            ]
        );
        let source = world
            .spawn(MorphWeights::new(vec![0.5, 0.0], None).unwrap())
            .id();
        world
            .entity_mut(entity)
            .insert(MeshMorphWeights::Reference(source));
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 2,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 2,
                    y: 1,
                    width: 2
                }
            ]
        );
        world.get_mut::<MorphWeights>(source).unwrap().weights_mut()[0] = 1.0;
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 4,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 4,
                    y: 1,
                    width: 2
                }
            ]
        );
        world.despawn(source);
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 0,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 0,
                    y: 1,
                    width: 2
                }
            ]
        );
    }

    #[test]
    fn morph_precedes_skin_and_zero_weights_do_not_normalize() {
        let mut shape = mesh(vec![[1.0, 0.0, 0.0]])
            .with_inserted_attribute(
                Mesh::ATTRIBUTE_JOINT_INDEX,
                VertexAttributeValues::Uint16x4(vec![[0, 7, 8, 9]]),
            )
            .with_inserted_attribute(Mesh::ATTRIBUTE_JOINT_WEIGHT, vec![[0.5, 0.0, 0.0, 0.0]]);
        shape.set_morph_targets(vec![MorphAttributes::new(Vec3::X, Vec3::ZERO, Vec3::ZERO)]);
        let source = SourceMesh::read(&shape).unwrap();
        let skin = [Mat4::from_rotation_z(std::f32::consts::FRAC_PI_2)];
        let posed = source
            .pose(
                0,
                &[1.0],
                Mat4::from_translation(Vec3::splat(99.0)),
                Some(&skin),
            )
            .unwrap();
        assert!(
            posed.distance(Vec3::Y) < 1e-6,
            "morph delta must rotate with the joint"
        );
        assert_eq!(source.pose(0, &[0.0], Mat4::IDENTITY, None), Some(Vec3::X));
        assert!(source.pose(0, &[f32::NAN], Mat4::IDENTITY, None).is_none());
        assert!(source.pose(0, &[], Mat4::IDENTITY, Some(&[])).is_none());
    }

    #[test]
    fn viewport_visibility_layers_and_material_culling_are_respected() {
        let mut world = test_world();
        world.init_resource::<Assets<MToonMaterial>>();
        let camera = world
            .query_filtered::<Entity, With<Camera3d>>()
            .single(&world)
            .unwrap();
        world.get_mut::<Camera>(camera).unwrap().viewport = Some(bevy::camera::Viewport {
            physical_position: UVec2::new(2, 1),
            physical_size: UVec2::new(4, 4),
            ..default()
        });
        let (entity, _) = spawn_mesh(&mut world, rectangle(0.0, 0.0, 8.0, 8.0));
        let expected: Vec<_> = (1..5).map(|y| Run { x: 2, y, width: 4 }).collect();
        assert_eq!(projected_runs(&mut world, 8, 8, 1), expected);
        world.entity_mut(entity).insert(RenderLayers::layer(1));
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        world.entity_mut(camera).insert(RenderLayers::layer(1));
        assert_eq!(projected_runs(&mut world, 8, 8, 1), expected);
        world.entity_mut(entity).insert(InheritedVisibility::HIDDEN);
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        world
            .entity_mut(entity)
            .insert(InheritedVisibility::VISIBLE);
        let material = world
            .resource_mut::<Assets<MToonMaterial>>()
            .add(MToonMaterial {
                cull_mode: Some(Face::Front),
                ..default()
            });
        world
            .entity_mut(entity)
            .insert(MeshMaterial3d(material.clone()));
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        world.get_mut::<Camera>(camera).unwrap().invert_culling = true;
        assert_eq!(projected_runs(&mut world, 8, 8, 1), expected);
        world
            .resource_mut::<Assets<MToonMaterial>>()
            .get_mut(&material)
            .unwrap()
            .cull_mode = None;
        assert_eq!(projected_runs(&mut world, 8, 8, 1), expected);
    }

    #[test]
    fn cache_invalidates_modified_assets_and_prunes_despawned_instances() {
        let mut world = test_world();
        let (entity, handle) = spawn_mesh(&mut world, rectangle(0.0, 0.0, 2.0, 2.0));
        let first = projected_runs(&mut world, 8, 8, 1);
        let pointer = world.resource::<GeometryCache>().meshes[&handle.id()]
            .positions
            .as_ptr();
        assert_eq!(projected_runs(&mut world, 8, 8, 1), first);
        assert_eq!(
            world.resource::<GeometryCache>().meshes[&handle.id()]
                .positions
                .as_ptr(),
            pointer
        );
        *world
            .resource_mut::<Assets<Mesh>>()
            .get_mut(&handle)
            .unwrap() = rectangle(4.0, 0.0, 6.0, 2.0);
        world
            .resource_mut::<Messages<AssetEvent<Mesh>>>()
            .write(AssetEvent::Modified { id: handle.id() });
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 4,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 4,
                    y: 1,
                    width: 2
                }
            ]
        );
        world.despawn(entity);
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        assert!(world.resource::<GeometryCache>().meshes.is_empty());
    }

    #[test]
    fn topology_handles_unindexed_lists_strips_restart_and_bad_indices() {
        let unindexed = mesh(vec![[0.0; 3]; 6]);
        assert_eq!(
            SourceMesh::read(&unindexed).unwrap().triangles,
            vec![[0, 1, 2], [3, 4, 5]]
        );
        let strip = Mesh::new(
            PrimitiveTopology::TriangleStrip,
            RenderAssetUsages::default(),
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, vec![[0.0; 3]; 7])
        .with_inserted_indices(Indices::U16(vec![0, 1, 2, 3, u16::MAX, 4, 5, 6]));
        assert_eq!(
            SourceMesh::read(&strip).unwrap().triangles,
            vec![[0, 1, 2], [2, 1, 3], [4, 5, 6]]
        );
        let bad = mesh(vec![[0.0; 3]; 3]).with_inserted_indices(Indices::U32(vec![0, 1, 99]));
        assert!(SourceMesh::read(&bad).unwrap().triangles.is_empty());
        let missing = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::default(),
        );
        assert!(SourceMesh::read(&missing).is_none());
    }

    #[test]
    fn empty_and_invalid_inputs_and_singular_camera_are_safe() {
        let mut world = World::new();
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        world = test_world();
        spawn_mesh(&mut world, rectangle(0.0, 0.0, 8.0, 8.0));
        for (w, h, s) in [(0, 8, 1), (8, 0, 1), (8, 8, 0), (u32::MAX, 8, 1)] {
            assert!(projected_runs(&mut world, w, h, s).is_empty());
        }
        let camera = world
            .query_filtered::<Entity, With<Camera3d>>()
            .single(&world)
            .unwrap();
        world
            .entity_mut(camera)
            .insert(GlobalTransform::from(Transform::from_scale(Vec3::ZERO)));
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
    }

    #[test]
    fn image_target_camera_is_preferred_over_unrelated_cameras() {
        let mut world = test_world();
        world.init_resource::<Assets<Image>>();
        let avatar_camera = world
            .query_filtered::<Entity, With<Camera3d>>()
            .single(&world)
            .unwrap();
        let image = world
            .resource_mut::<Assets<Image>>()
            .add(Image::new_target_texture(
                8,
                8,
                bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
                None,
            ));
        world
            .entity_mut(avatar_camera)
            .insert(RenderTarget::Image(image.into()));
        let projection = world.get::<Projection>(avatar_camera).unwrap().clone();
        world.spawn((
            Camera3d::default(),
            Camera {
                order: 100,
                ..default()
            },
            projection,
            GlobalTransform::from(Transform::from_xyz(100.0, 0.0, 0.0)),
        ));
        spawn_mesh(&mut world, rectangle(0.0, 0.0, 2.0, 2.0));
        assert_eq!(
            projected_runs(&mut world, 8, 8, 1),
            vec![
                Run {
                    x: 0,
                    y: 0,
                    width: 2
                },
                Run {
                    x: 0,
                    y: 1,
                    width: 2
                },
            ]
        );
        world.get_mut::<Camera>(avatar_camera).unwrap().is_active = false;
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
    }

    #[test]
    fn unavailable_cpu_data_is_skipped_without_panicking_or_changing_usage() {
        let mut world = test_world();
        let mut shape = rectangle(0.0, 0.0, 2.0, 2.0);
        shape.asset_usage = RenderAssetUsages::RENDER_WORLD;
        // Simulate extraction of an asset without MAIN_WORLD. The fallible
        // accessors must skip it even on the very first candidate call.
        let _gpu_data = shape.take_gpu_data().unwrap();
        assert!(SourceMesh::read(&shape).is_none());
        let (_, handle) = spawn_mesh(&mut world, shape);
        assert!(projected_runs(&mut world, 8, 8, 1).is_empty());
        assert_eq!(
            world
                .resource::<Assets<Mesh>>()
                .get(&handle)
                .unwrap()
                .asset_usage,
            RenderAssetUsages::RENDER_WORLD
        );
        let (_, retained) = spawn_mesh(&mut world, rectangle(4.0, 0.0, 6.0, 2.0));
        assert!(!projected_runs(&mut world, 8, 8, 1).is_empty());
        assert_eq!(
            world
                .resource::<Assets<Mesh>>()
                .get(&retained)
                .unwrap()
                .asset_usage,
            RenderAssetUsages::default()
        );
    }
}
