use super::*;
use crate::character::Notification;
use serde_json::json;

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
fn placement_hit_regions_and_stacking_follow_the_visible_scene() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, TransformPlugin, setup))
        .add_message::<Notification>()
        .configure_sets(
            PostUpdate,
            VisibilitySystems::CalculateBounds.after(bevy::transform::TransformSystems::Propagate),
        );
    let screen = output(&mut app, "screen", Vec2::ZERO, 2.);
    let mut actors = Vec::new();
    for count in [1, 51, 128, 2, 0] {
        while actors.len() < count {
            let mut character = Character::new(actors.len().to_string(), default());
            character.state = AvatarState::Ready;
            let actor = app.world_mut().spawn(character).id();
            app.world_mut().spawn((
                Mesh3d(Handle::default()),
                Aabb::from_min_max(Vec3::ZERO, Vec3::splat(0.1)),
                Transform::default(),
                InheritedVisibility::VISIBLE,
                ChildOf(actor),
            ));
            actors.push(actor);
        }
        while actors.len() > count {
            app.world_mut().despawn(actors.pop().unwrap());
        }
        app.update();
        for &actor in &actors {
            let transform = app.world().get::<Transform>(actor).unwrap();
            assert_eq!(transform.translation.truncate(), Vec2::new(100., -160.));
            assert_eq!(transform.scale, Vec3::splat(320.));
            let camera = app.world().get::<Transform>(screen).unwrap();
            let Projection::Orthographic(projection) =
                app.world().get::<Projection>(screen).unwrap()
            else {
                panic!()
            };
            let distance = camera.translation.z - transform.translation.z;
            assert!(distance - 320. > projection.near && distance + 320. < projection.far);
        }
        if let Some(&front) = actors.iter().max_by_key(|e| e.to_bits()) {
            let expected = app.world().get::<Character>(front).unwrap().id.clone();
            for button in [true, false] {
                app.world_mut().write_message(PointerInput {
                    output: screen,
                    local: Vec2::new(55., 75.),
                    button: Some(button),
                });
            }
            app.update();
            let events: Vec<_> = app
                .world_mut()
                .resource_mut::<Messages<Notification>>()
                .drain()
                .map(|v| v.0)
                .collect();
            assert_eq!(events, [json!({"event":"click","character":expected})]);
        }
    }
    assert!(app.world().get::<HitRegions>(screen).unwrap().0.is_empty());
    assert!(
        app.world().get::<Camera>(screen).unwrap().is_active,
        "clear the previous frame"
    );
    app.update();
    assert!(!app.world().get::<Camera>(screen).unwrap().is_active);
    assert_eq!(
        app.world().get::<Transform>(screen).unwrap().translation.z,
        50_000.
    );
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
