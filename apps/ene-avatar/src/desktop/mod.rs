//! Desktop output placement, cameras, hit regions, and pointer interaction.

mod pointer;
#[cfg(any(windows, test))]
pub(crate) use pointer::pointer;
pub(crate) use pointer::{NativeInput, cancel_pointer};

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
    app.add_plugins(pointer::setup)
        .add_systems(Update, (cameras, place_avatars, depth_range).chain())
        .add_systems(Update, sync_virtual_time)
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
type CameraDepthQuery<'w, 's> = Query<
    'w,
    's,
    (&'static mut Transform, &'static mut Projection),
    (With<Output>, Without<Character>),
>;

fn depth_range(avatars: Query<&Transform, With<Character>>, mut cameras: CameraDepthQuery<'_, '_>) {
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

#[cfg(any(windows, test))]
impl HitRegions {
    /// Tests the Windows cursor's four-logical-pixel cell against projected
    /// mesh bounds. The empty-grid case keeps precise region hit testing.
    pub(crate) fn contains_cell(&self, logical_size: Vec2, point: Vec2) -> bool {
        let exact = || self.0.iter().any(|(_, rect)| rect.contains(point));
        if !logical_size.is_finite() || !logical_size.cmpgt(Vec2::ZERO).all() {
            return exact();
        }

        let cell = (point / 4.).floor();
        let limit = (logical_size / 4.)
            .ceil()
            .clamp(Vec2::ONE, Vec2::splat(4096.));
        let mut occupied = false;
        for (_, rect) in &self.0 {
            let start = (rect.min / 4.).floor().clamp(Vec2::ZERO, limit);
            let end = (rect.max / 4.).ceil().clamp(Vec2::ZERO, limit);
            if start.cmplt(end).all() {
                occupied = true;
                if cell.cmpge(start).all() && cell.cmplt(end).all() {
                    return true;
                }
            }
        }
        !occupied && exact()
    }
}

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
                let matrix = transform.to_matrix();
                let center = matrix.transform_point3(aabb.center.into());
                let half = Mat3::from_mat4(matrix).abs() * Vec3::from(aabb.half_extents);
                Some((owner, Vec2::new(center.x, -center.y), half.truncate()))
            }),
    );
    for (output, mut camera, mut regions) in &mut outputs {
        let had = !regions.0.is_empty();
        next.clear();
        let limit = output.size / output.units_per_logical_pixel;
        for &(actor, center, half) in geometry.iter() {
            let center = (center - output.origin) / output.units_per_logical_pixel;
            let half = half / output.units_per_logical_pixel + Vec2::splat(2.);
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
    use bevy::picking::{
        events::PointerState,
        hover::PointerCaptureMap,
        pointer::{PointerAction, PointerButton, PointerId, PointerMap, PointerPressState},
    };

    const PRESS: PointerAction = PointerAction::Press(PointerButton::Primary);
    const RELEASE: PointerAction = PointerAction::Release(PointerButton::Primary);
    const MOVE: PointerAction = PointerAction::Move { delta: Vec2::ZERO };

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
        assert_eq!(
            app.world().get::<HitRegions>(screen).unwrap().0,
            vec![(
                actor,
                Rect::from_corners(Vec2::new(48., 47.), Vec2::new(53., 52.))
            )]
        );
        app.world_mut().get_mut::<Character>(actor).unwrap().state = AvatarState::Suspended;
        app.update();
        assert_eq!(app.world().get::<HitRegions>(screen).unwrap().0.len(), 0);
        assert!(
            app.world().get::<Camera>(screen).unwrap().is_active,
            "clear the previous image once"
        );
        app.update();
        assert!(!app.world().get::<Camera>(screen).unwrap().is_active);
        app.world_mut().get_mut::<Character>(actor).unwrap().state = AvatarState::Ready;
        app.update();
        assert!(app.world().get::<Camera>(screen).unwrap().is_active);
        assert_ne!(app.world().get::<HitRegions>(screen).unwrap().0.len(), 0);
    }

    #[test]
    fn regions_preserve_padding_clipping_and_visibility_gate() {
        let mut app = App::new();
        app.add_systems(Update, regions);
        let screen = output(&mut app, "left", Vec2::ZERO, 1.);
        app.world_mut().entity_mut(screen).insert(Camera::default());
        let actor = actor(&mut app, "a");
        let mesh = app
            .world_mut()
            .spawn((
                Mesh3d::default(),
                Aabb::from_min_max(Vec3::ZERO, Vec3::splat(2.)),
                GlobalTransform::default(),
                InheritedVisibility::VISIBLE,
                ChildOf(actor),
            ))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<HitRegions>(screen).unwrap().0,
            vec![(actor, Rect::from_corners(Vec2::ZERO, Vec2::new(4., 2.)))]
        );
        app.world_mut()
            .entity_mut(mesh)
            .insert(InheritedVisibility::HIDDEN);
        app.update();
        assert_eq!(app.world().get::<HitRegions>(screen).unwrap().0.len(), 0);
    }

    #[test]
    fn dragging_preserves_logical_offset_across_dpi() {
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
        for (screen, local, action) in [
            (left, Vec2::new(10., 20.), PRESS),
            (right, Vec2::new(5., 10.), RELEASE),
        ] {
            app.world_mut()
                .resource_mut::<NativeInput>()
                .send(screen, local, action);
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
        assert_released(&app);
    }

    fn interaction_app() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, pointer::setup))
            .add_message::<Notification>();
        let screen = output(&mut app, "left", Vec2::ZERO, 1.);
        let actor = actor(&mut app, "a");
        app.world_mut().get_mut::<HitRegions>(screen).unwrap().0 =
            vec![(actor, Rect::from_corners(Vec2::ZERO, Vec2::splat(100.)))];
        (app, screen, actor)
    }

    fn actor(app: &mut App, id: &str) -> Entity {
        let mut character = Character::new(
            id.into(),
            Config {
                position: Some([50., 50.]),
                ..default()
            },
        );
        character.state = AvatarState::Ready;
        app.world_mut()
            .spawn((character, Transform::from_xyz(50., -50., 0.)))
            .id()
    }

    fn send(app: &mut App, output: Entity, position: Vec2, action: PointerAction) {
        app.world_mut()
            .resource_mut::<NativeInput>()
            .send(output, position, action);
    }

    fn notifications(app: &mut App) -> Vec<serde_json::Value> {
        app.world_mut()
            .resource_mut::<Messages<Notification>>()
            .drain()
            .map(|notification| serde_json::to_value(notification.0).unwrap())
            .collect()
    }

    fn assert_released(app: &App) {
        let world = app.world();
        assert!(
            !world
                .resource::<PointerCaptureMap>()
                .is_captured(&PointerId::Mouse)
        );
        let pointer = world
            .resource::<PointerMap>()
            .get_entity(PointerId::Mouse)
            .unwrap();
        assert!(
            !world
                .get::<PointerPressState>(pointer)
                .unwrap()
                .is_any_pressed()
        );
        if let Some(state) = world
            .resource::<PointerState>()
            .get(PointerId::Mouse, PointerButton::Primary)
        {
            assert_eq!(state.pressing.len(), 0);
            assert_eq!(state.dragging.len(), 0);
        }
    }

    #[test]
    fn first_sample_hits_and_four_pixels_remains_a_click() {
        let (mut app, screen, actor) = interaction_app();
        send(&mut app, screen, Vec2::splat(50.), PRESS);
        send(&mut app, screen, Vec2::new(54., 50.), RELEASE);
        app.update();
        assert_eq!(
            notifications(&mut app),
            vec![serde_json::json!({"event":"click", "character":"a"})]
        );
        assert_eq!(
            app.world().get::<Character>(actor).unwrap().config.position,
            Some([50., 50.])
        );
        assert_released(&app);
    }

    #[test]
    fn subthreshold_release_outside_bounds_preserves_captured_click_and_next_gesture() {
        for scale in [1., 1.5, 2.] {
            for separate_updates in [false, true] {
                let (mut app, screen, actor) = interaction_app();
                app.world_mut()
                    .get_mut::<Output>(screen)
                    .unwrap()
                    .units_per_logical_pixel = scale;
                let bounds = app.world().get::<HitRegions>(screen).unwrap().0[0].1;
                let press = Vec2::new(99., 50.);
                let release = Vec2::new(102., 50.);
                assert!(bounds.contains(press));
                assert!(!bounds.contains(release));
                assert!(press.distance(release) <= 4.);
                send(&mut app, screen, press, PRESS);
                if separate_updates {
                    app.update();
                    assert_eq!(notifications(&mut app).len(), 0);
                    assert!(
                        app.world()
                            .resource::<PointerCaptureMap>()
                            .is_captured(&PointerId::Mouse)
                    );
                }
                send(&mut app, screen, release, RELEASE);
                app.update();
                assert_eq!(
                    notifications(&mut app),
                    vec![serde_json::json!({"event":"click", "character":"a"})]
                );
                assert_released(&app);
                for action in [PRESS, RELEASE] {
                    send(&mut app, screen, Vec2::splat(50.), action);
                    if separate_updates {
                        app.update();
                    }
                }
                if !separate_updates {
                    app.update();
                }
                assert_eq!(
                    notifications(&mut app),
                    vec![serde_json::json!({"event":"click", "character":"a"})]
                );
                let character = app.world().get::<Character>(actor).unwrap();
                assert_eq!(character.config.position, Some([50., 50.]));
                assert_eq!(character.config.monitor, None);
                assert_eq!(
                    app.world().get::<Transform>(actor).unwrap().translation,
                    Vec3::new(50., -50., 0.)
                );
                assert_released(&app);
            }
        }
    }

    #[test]
    fn dragging_latches_above_four_pixels_and_release_alone_can_move() {
        for positions in [
            vec![Vec2::new(55., 50.), Vec2::splat(50.)],
            vec![Vec2::new(80., 50.)],
        ] {
            let (mut app, screen, actor) = interaction_app();
            send(&mut app, screen, Vec2::splat(50.), PRESS);
            for &position in &positions[..positions.len() - 1] {
                send(&mut app, screen, position, MOVE);
            }
            send(&mut app, screen, *positions.last().unwrap(), RELEASE);
            app.update();
            assert_eq!(
                app.world().get::<Character>(actor).unwrap().config.position,
                Some(positions.last().unwrap().to_array())
            );
            assert_eq!(notifications(&mut app).len(), 0);
            assert_released(&app);
        }
    }

    #[test]
    fn drag_latch_survives_updates_and_a_new_press_can_click() {
        let (mut app, screen, _) = interaction_app();
        for (position, action) in [
            (Vec2::splat(50.), PRESS),
            (Vec2::new(55., 50.), MOVE),
            (Vec2::splat(50.), MOVE),
            (Vec2::splat(50.), RELEASE),
        ] {
            send(&mut app, screen, position, action);
            app.update();
        }
        assert_eq!(notifications(&mut app).len(), 0);
        for action in [PRESS, RELEASE] {
            send(&mut app, screen, Vec2::splat(50.), action);
        }
        app.update();
        assert_eq!(
            notifications(&mut app),
            vec![serde_json::json!({"event":"click", "character":"a"})]
        );
        assert_released(&app);
    }

    #[test]
    fn queued_drag_keeps_the_first_hit_and_ignores_later_targets() {
        let (mut app, screen, first) = interaction_app();
        let second = actor(&mut app, "b");
        app.world_mut().get_mut::<HitRegions>(screen).unwrap().0 = vec![
            (first, Rect::from_corners(Vec2::ZERO, Vec2::splat(40.))),
            (
                second,
                Rect::from_corners(Vec2::splat(60.), Vec2::splat(100.)),
            ),
        ];
        send(&mut app, screen, Vec2::splat(20.), MOVE);
        send(&mut app, screen, Vec2::splat(20.), PRESS);
        send(&mut app, screen, Vec2::splat(80.), MOVE);
        send(&mut app, screen, Vec2::splat(80.), RELEASE);
        app.update();
        assert_eq!(
            app.world().get::<Character>(first).unwrap().config.position,
            Some([110., 110.])
        );
        assert_eq!(
            app.world()
                .get::<Character>(second)
                .unwrap()
                .config
                .position,
            Some([50., 50.])
        );
        assert_eq!(notifications(&mut app).len(), 0);
        assert_released(&app);
    }

    #[test]
    fn backend_delegates_depth_blocking_and_deduplicates_mesh_regions() {
        let (mut app, screen, first) = interaction_app();
        let second = actor(&mut app, "b");
        let (front, back) = if first.to_bits() > second.to_bits() {
            (first, second)
        } else {
            (second, first)
        };
        let rect = Rect::from_corners(Vec2::ZERO, Vec2::splat(100.));
        app.world_mut().get_mut::<HitRegions>(screen).unwrap().0 =
            vec![(front, rect), (back, rect), (front, rect)];
        for action in [PRESS, RELEASE] {
            send(&mut app, screen, Vec2::splat(50.), action);
        }
        app.update();
        let expected = app.world().get::<Character>(front).unwrap().id.clone();
        assert_eq!(
            notifications(&mut app),
            vec![serde_json::json!({"event":"click", "character":expected})]
        );
        app.world_mut()
            .entity_mut(front)
            .insert(bevy::picking::Pickable::IGNORE);
        for action in [PRESS, RELEASE] {
            send(&mut app, screen, Vec2::splat(50.), action);
        }
        app.update();
        let expected = app.world().get::<Character>(back).unwrap().id.clone();
        assert_eq!(
            notifications(&mut app),
            vec![serde_json::json!({"event":"click", "character":expected})]
        );
    }

    #[test]
    fn identical_local_coordinates_cross_outputs_and_preserve_logical_offset() {
        let (mut app, left, actor) = interaction_app();
        app.world_mut().get_mut::<Output>(left).unwrap().origin = Vec2::new(-100., 0.);
        app.world_mut()
            .get_mut::<Transform>(actor)
            .unwrap()
            .translation
            .x = -50.;
        let right = output(&mut app, "right", Vec2::ZERO, 2.);
        send(&mut app, left, Vec2::new(10., 20.), PRESS);
        send(&mut app, right, Vec2::new(10., 20.), RELEASE);
        app.update();
        let config = &app.world().get::<Character>(actor).unwrap().config;
        assert_eq!(config.monitor.as_deref(), Some("right"));
        assert_eq!(config.position, Some([50., 50.]));
        assert_eq!(notifications(&mut app).len(), 0);
        assert_released(&app);
    }

    #[test]
    fn cancellation_and_removal_clear_library_capture_and_press_without_phantom_clicks() {
        for cause in 0..4 {
            let (mut app, screen, actor) = interaction_app();
            send(&mut app, screen, Vec2::splat(50.), PRESS);
            app.update();
            assert!(
                app.world()
                    .resource::<PointerCaptureMap>()
                    .is_captured(&PointerId::Mouse)
            );
            match cause {
                0 => {
                    send(&mut app, screen, Vec2::splat(500.), MOVE);
                    cancel_pointer(app.world_mut());
                }
                1 => {
                    app.world_mut().despawn(actor);
                }
                2 => {
                    app.world_mut().despawn(screen);
                }
                _ => {
                    cancel_pointer(app.world_mut());
                }
            }
            app.update();
            assert_released(&app);
            let position = app
                .world()
                .get::<Character>(actor)
                .map(|character| character.config.position);
            send(&mut app, screen, Vec2::splat(50.), RELEASE);
            app.update();
            assert_eq!(
                app.world()
                    .get::<Character>(actor)
                    .map(|character| character.config.position),
                position
            );
            assert_eq!(notifications(&mut app).len(), 0);
            assert_released(&app);
        }
    }

    #[test]
    fn native_cancellation_between_queued_press_and_release_stays_ordered() {
        let (mut app, screen, _) = interaction_app();
        send(&mut app, screen, Vec2::splat(50.), PRESS);
        cancel_pointer(app.world_mut());
        send(&mut app, screen, Vec2::splat(50.), RELEASE);
        app.update();
        assert_eq!(notifications(&mut app).len(), 0);
        assert_released(&app);
    }

    #[test]
    fn nonprimary_buttons_and_empty_regions_do_not_click() {
        let (mut app, screen, actor) = interaction_app();
        for action in [
            PointerAction::Press(PointerButton::Secondary),
            PointerAction::Release(PointerButton::Secondary),
        ] {
            send(&mut app, screen, Vec2::splat(50.), action);
        }
        for action in [PRESS, RELEASE] {
            send(&mut app, screen, Vec2::splat(150.), action);
        }
        app.world_mut().get_mut::<Character>(actor).unwrap().state = AvatarState::Suspended;
        for action in [PRESS, RELEASE] {
            send(&mut app, screen, Vec2::splat(50.), action);
        }
        app.update();
        assert_eq!(notifications(&mut app).len(), 0);
        assert_released(&app);
    }

    #[test]
    fn projected_mesh_regions_keep_windows_cell_edges_and_gaps() {
        let regions = HitRegions(vec![
            (
                Entity::from_bits(1),
                Rect::from_corners(Vec2::new(21., 21.), Vec2::new(59., 119.)),
            ),
            (
                Entity::from_bits(2),
                Rect::from_corners(Vec2::new(200., 40.), Vec2::new(260., 140.)),
            ),
        ]);
        let size = Vec2::new(400., 300.);
        for (point, expected) in [
            (Vec2::new(40., 60.), true),
            (Vec2::new(220., 80.), true),
            (Vec2::new(20., 20.), true),
            (Vec2::new(59.9, 119.9), true),
            (Vec2::new(60., 120.), false),
            (Vec2::new(120., 70.), false),
            (Vec2::new(-1., 70.), false),
            (Vec2::new(f32::NAN, 80.), false),
        ] {
            assert_eq!(regions.contains_cell(size, point), expected, "{point:?}");
        }
        assert!(!HitRegions::default().contains_cell(size, Vec2::new(40., 60.)));
        assert!(regions.contains_cell(Vec2::ZERO, Vec2::new(40., 60.)));
        assert!(!regions.contains_cell(Vec2::ZERO, Vec2::new(20., 20.)));
    }
}
