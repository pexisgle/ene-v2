//! Desktop output placement, cameras, hit regions, and pointer interaction.

mod pointer;
pub(crate) use pointer::{Drag, PointerInput, pointer};

use crate::character::{AvatarState, Character, Config};
use bevy::{camera::visibility::VisibilitySystems, prelude::*};
use bevy::{
    camera::{RenderTarget, ScalingMode, primitives::Aabb},
    window::{CompositeAlphaMode, PresentMode, WindowRef, WindowResolution},
};

pub fn setup(app: &mut App) {
    // 1 FPS needs a full second plus scheduling jitter before the stall cap.
    app.insert_resource(Time::<Virtual>::from_max_delta(
        std::time::Duration::from_secs(2),
    ));
    app.init_resource::<Drag>()
        .add_message::<PointerInput>()
        .add_systems(PreUpdate, pointer)
        .add_systems(Update, (cameras, place_avatars, depth_range).chain())
        .add_systems(Update, sync_virtual_time)
        .add_observer(|_: On<Remove<Output>>, mut drag: ResMut<Drag>| drag.entity = None)
        .add_observer(|event: On<Remove<Character>>, mut drag: ResMut<Drag>| {
            if drag.entity == Some(event.entity) {
                drag.entity = None;
            }
        })
        .add_systems(
            PostUpdate,
            regions
                .after(VisibilitySystems::CalculateBounds)
                .after(VisibilitySystems::VisibilityPropagate)
                .before(VisibilitySystems::CheckVisibility),
        );
}

/// No output surfaces means no simulation; the Wayland runner only translates
/// compositor events.
fn sync_virtual_time(outputs: Query<&Output>, mut time: ResMut<Time<Virtual>>) {
    if outputs.is_empty() {
        time.pause();
    } else {
        time.unpause();
    }
}

#[derive(Component, Clone, PartialEq)]
#[require(HitRegions, Transform, Visibility)]
pub struct Output {
    pub name: String,
    pub origin: Vec2,
    pub size: Vec2,
    pub units_per_logical_pixel: f32,
}

pub fn bounds(output: &Output) -> Rect {
    Rect::from_corners(output.origin, output.origin + output.size)
}

pub fn desktop_position(output: &Output, local: Vec2) -> Vec2 {
    output.origin + local * output.units_per_logical_pixel
}

/// The monitor a placement resolves to: the named one, or the first output.
pub fn pick_output<'a>(
    placement: &Config,
    mut outputs: impl Iterator<Item = &'a Output>,
) -> Option<&'a Output> {
    let first = outputs.next()?;
    std::iter::once(first)
        .chain(outputs)
        .find(|output| placement.monitor.as_deref() == Some(output.name.as_str()))
        .or(Some(first))
}

/// The requested logical position, or the default on the selected output.
pub fn resolved_position(placement: &Config, output: Option<&Output>) -> Option<Vec2> {
    placement.position.map(Vec2::from_array).or_else(|| {
        output.map(|output| output.size / output.units_per_logical_pixel * Vec2::new(0.5, 0.8))
    })
}

pub fn window(output: &Output) -> Window {
    let mut resolution = WindowResolution::new(output.size.x as u32, output.size.y as u32);
    resolution.set_scale_factor(output.units_per_logical_pixel);
    Window {
        title: "ene-avatar".into(),
        transparent: true,
        resolution,
        composite_alpha_mode: CompositeAlphaMode::PreMultiplied,
        present_mode: PresentMode::AutoNoVsync,
        ..default()
    }
}

/// Derives placement and stacking depth; unchanged transforms stay untouched.
#[allow(clippy::type_complexity)]
fn place_avatars(
    outputs: Query<&Output>,
    mut avatars: Query<(Entity, &Character, &mut Transform)>,
) {
    for (index, (_, character, mut transform)) in avatars
        .iter_mut()
        .sort_unstable_by_key::<Entity, _>(|entity| entity.to_bits())
        .enumerate()
    {
        let placement = &character.config;
        let output = pick_output(placement, outputs.iter());
        let (position, units_per_logical_pixel) = output.map_or((Vec2::ZERO, 1.), |output| {
            (
                desktop_position(
                    output,
                    resolved_position(placement, Some(output)).unwrap_or_default(),
                ),
                output.units_per_logical_pixel,
            )
        });
        let next = Transform::from_xyz(position.x, -position.y, index as f32 * 1000.).with_scale(
            Vec3::splat(320. * placement.size_percent / 100. * units_per_logical_pixel),
        );
        transform.set_if_neq(next);
    }
}

fn cameras(mut commands: Commands, outputs: Query<(Entity, &Output), Changed<Output>>) {
    for (entity, output) in &outputs {
        let center = output.origin + output.size * 0.5;
        commands.entity(entity).insert((
            Camera3d::default(),
            // Small avatar scenes do not amortize indirect draw preparation.
            bevy::render::view::NoIndirectDrawing,
            bevy::light::cluster::ClusterConfig::Single,
            Projection::Orthographic(OrthographicProjection {
                scaling_mode: ScalingMode::Fixed {
                    width: output.size.x,
                    height: output.size.y,
                },
                near: 0.0,
                far: 100_000.0,
                ..OrthographicProjection::default_3d()
            }),
            RenderTarget::Window(WindowRef::Entity(entity)),
            Transform::from_xyz(center.x, -center.y, 50_000.0),
            Msaa::Off,
            crate::rendering::EXPOSURE,
            bevy::core_pipeline::tonemapping::Tonemapping::Linear,
        ));
    }
}

/// Keep the same depth margin around the entire stack as around a single avatar.
/// Orthographic projection preserves XY placement and apparent size as Z moves.
#[allow(clippy::type_complexity)]
fn depth_range(
    avatars: Query<&Transform, With<Character>>,
    mut cameras: Query<(&mut Transform, &mut Projection), (With<Output>, Without<Character>)>,
) {
    let front = avatars
        .iter()
        .map(|transform| transform.translation.z)
        .fold(0.0_f32, f32::max);
    for (mut transform, mut projection) in &mut cameras {
        let z = 50_000.0 + front;
        if transform.translation.z != z {
            transform.translation.z = z;
        }
        if let Projection::Orthographic(orthographic) = &*projection
            && orthographic.far != 100_000.0 + front
            && let Projection::Orthographic(orthographic) = &mut *projection
        {
            orthographic.far = 100_000.0 + front;
        }
    }
}

#[derive(Component, Default)]
pub struct HitRegions(pub Vec<(Entity, Rect)>);

fn regions(
    mut outputs: Query<(&Output, &mut Camera, &mut HitRegions)>,
    avatars: Query<(Entity, &Character)>,
    parents: Query<&ChildOf>,
    meshes: Query<(Entity, &Aabb, &GlobalTransform, &InheritedVisibility), With<Mesh3d>>,
    mut geometry: Local<Vec<(Entity, Vec2, Vec2)>>,
    mut next: Local<Vec<(Entity, Rect)>>,
) {
    geometry.clear();
    geometry.extend(
        meshes
            .iter()
            .filter_map(|(entity, aabb, transform, visible)| {
                let (owner, character) = avatars
                    .iter_many(std::iter::once(entity).chain(parents.iter_ancestors(entity)))
                    .matched()
                    .next()?;
                if character.state != AvatarState::Ready || !visible.get() {
                    return None;
                }
                // shortcut: mesh bounds include gaps and omit morph deformation; refine if input misses are observed.
                let matrix = transform.to_matrix();
                let center = matrix.transform_point3(aabb.center.into());
                let half = Mat3::from_mat4(matrix).abs() * Vec3::from(aabb.half_extents);
                Some((owner, Vec2::new(center.x, -center.y), half.truncate()))
            }),
    );
    for (output, mut camera, mut regions) in &mut outputs {
        let had = !regions.0.is_empty();
        next.clear();
        for &(actor, center, half) in geometry.iter() {
            let center = (center - output.origin) / output.units_per_logical_pixel;
            let half = half / output.units_per_logical_pixel + Vec2::splat(2.);
            let limit = output.size / output.units_per_logical_pixel;
            let min = (center - half).max(Vec2::ZERO);
            let max = (center + half).min(limit);
            if min.is_finite() && max.is_finite() && min.cmplt(max).all() {
                next.push((actor, Rect::from_corners(min, max)));
            }
        }
        // Clear the previous image once before disabling an empty camera.
        let active = had || !next.is_empty();
        if regions.0 != *next {
            std::mem::swap(&mut regions.0, &mut next);
        }
        if camera.is_active != active {
            camera.is_active = active;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::character::Notification;

    fn output(app: &mut App, name: &str, origin: Vec2, scale: f32) -> Entity {
        app.world_mut()
            .spawn(Output {
                name: name.into(),
                origin,
                size: Vec2::splat(100. * scale),
                units_per_logical_pixel: scale,
            })
            .id()
    }

    #[test]
    fn regions_clear_the_last_frame_and_reactivate_when_an_avatar_returns() {
        let mut app = App::new();
        app.add_systems(Update, regions);
        let screen = app
            .world_mut()
            .spawn((
                Output {
                    name: "test".into(),
                    origin: Vec2::ZERO,
                    size: Vec2::splat(100.),
                    units_per_logical_pixel: 1.,
                },
                Camera::default(),
            ))
            .id();
        let mut character = Character::new("a".into(), Config::default());
        character.state = AvatarState::Ready;
        let actor = app.world_mut().spawn(character).id();
        app.world_mut().spawn((
            Mesh3d::default(),
            Aabb::from_min_max(Vec3::ZERO, Vec3::ONE),
            GlobalTransform::from_translation(Vec3::new(50., -50., 0.)),
            InheritedVisibility::VISIBLE,
            ChildOf(actor),
        ));
        app.update();
        assert!(!app.world().get::<HitRegions>(screen).unwrap().0.is_empty());
        app.world_mut().get_mut::<Character>(actor).unwrap().state = AvatarState::Suspended;
        app.update();
        assert!(app.world().get::<HitRegions>(screen).unwrap().0.is_empty());
        assert!(
            app.world().get::<Camera>(screen).unwrap().is_active,
            "clear the previous image once"
        );
        app.update();
        assert!(!app.world().get::<Camera>(screen).unwrap().is_active);
        app.world_mut().get_mut::<Character>(actor).unwrap().state = AvatarState::Ready;
        app.update();
        assert!(app.world().get::<Camera>(screen).unwrap().is_active);
        assert!(!app.world().get::<HitRegions>(screen).unwrap().0.is_empty());
    }

    #[test]
    fn dragging_preserves_logical_offset_across_dpi_and_cancels_on_removal() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, setup))
            .add_message::<Notification>();
        let left = output(&mut app, "left", Vec2::ZERO, 1.);
        let right = output(&mut app, "right", Vec2::new(100., 0.), 2.);
        let mut character = Character::new(
            "a".into(),
            Config {
                position: Some([50., 50.]),
                ..default()
            },
        );
        character.state = AvatarState::Ready;
        let actor = app.world_mut().spawn(character).id();
        app.update();
        app.world_mut().get_mut::<HitRegions>(left).unwrap().0 =
            vec![(actor, Rect::from_corners(Vec2::ZERO, Vec2::splat(100.)))];
        for (screen, local, button) in [
            (left, Vec2::new(10., 20.), true),
            (right, Vec2::new(5., 10.), false),
        ] {
            app.world_mut().write_message(PointerInput {
                output: screen,
                local,
                button: Some(button),
            });
            app.update();
        }
        let config = &app.world().get::<Character>(actor).unwrap().config;
        assert_eq!(config.monitor.as_deref(), Some("right"));
        assert_eq!(config.position, Some([45., 40.]));
        assert!(
            app.world_mut()
                .resource_mut::<Messages<Notification>>()
                .drain()
                .next()
                .is_none()
        );
        assert!(app.world().resource::<Drag>().entity.is_none());
        for entity in [right, actor] {
            app.world_mut().resource_mut::<Drag>().entity = Some(actor);
            app.world_mut().despawn(entity);
            assert!(app.world().resource::<Drag>().entity.is_none());
        }
    }
}
