//! Windows output ownership, independent of winit's current-monitor relation.

use crate::desktop::{self, Output};
use bevy::{
    prelude::*,
    window::{CursorOptions, OnMonitor, WindowLevel, WindowPosition},
};

#[derive(Component)]
pub(super) struct OutputMonitor(Entity);

pub(super) fn outputs(
    In(snapshots): In<Vec<(Entity, Output)>>,
    mut outputs: Query<(Entity, &OutputMonitor, &mut Output, &mut Window)>,
    mut commands: Commands,
) {
    // Winit detaches windows before removing a monitor and may move them to
    // another monitor. Their assigned output must not follow that reassignment.
    for (entity, owner, _, _) in &outputs {
        if !snapshots.iter().any(|(monitor, _)| *monitor == owner.0) {
            commands.entity(entity).despawn();
        }
    }
    for (monitor, output) in snapshots {
        if let Some((_, _, mut previous, mut window)) = outputs
            .iter_mut()
            .find(|(_, owner, _, _)| owner.0 == monitor)
        {
            if *previous != output {
                window.position = WindowPosition::At(output.origin.as_ivec2());
                window.resolution = desktop::window(&output).resolution;
                *previous = output;
            }
            continue;
        }
        commands.spawn((
            Window {
                decorations: false,
                focused: false,
                skip_taskbar: true,
                resizable: false,
                window_level: WindowLevel::AlwaysOnTop,
                position: WindowPosition::At(output.origin.as_ivec2()),
                ..desktop::window(&output)
            },
            CursorOptions {
                hit_test: false,
                ..default()
            },
            OnMonitor(monitor),
            OutputMonitor(monitor),
            output,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::{
        ecs::relationship::RelationshipTarget,
        window::{HasWindows, Monitor},
    };

    #[test]
    fn disconnected_outputs_are_removed_even_after_winit_reassigns_windows() {
        #[derive(Resource, Default)]
        struct LiveMonitors(Vec<(Entity, Output)>);
        fn read_monitors(monitors: Res<LiveMonitors>) -> Vec<(Entity, Output)> {
            monitors.0.clone()
        }
        fn output(name: &str) -> Output {
            Output {
                name: name.into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            }
        }
        fn window_for(app: &mut App, monitor: Entity) -> Entity {
            app.world_mut()
                .query::<(Entity, &OutputMonitor)>()
                .iter(app.world())
                .find(|(_, owner)| owner.0 == monitor)
                .unwrap()
                .0
        }
        fn count(app: &mut App) -> usize {
            app.world_mut().query::<&Output>().iter(app.world()).count()
        }
        // Match create_monitors in bevy_winit: it deliberately preserves windows.
        fn disconnect(app: &mut App, monitor: Entity) {
            app.world_mut().entity_mut(monitor).remove::<HasWindows>();
            app.world_mut().flush();
            app.world_mut().despawn(monitor);
            app.world_mut()
                .resource_mut::<LiveMonitors>()
                .0
                .retain(|(entity, _)| *entity != monitor);
        }

        let mut app = App::new();
        app.add_plugins((MinimalPlugins, desktop::setup))
            .add_message::<crate::character::Notification>()
            .init_resource::<LiveMonitors>()
            .add_systems(
                PreUpdate,
                read_monitors.pipe(outputs).before(desktop::pointer),
            );
        let left = app.world_mut().spawn_empty().id();
        let right = app.world_mut().spawn_empty().id();
        app.world_mut().resource_mut::<LiveMonitors>().0 =
            vec![(left, output("left")), (right, output("right"))];
        app.update();
        let left_window = window_for(&mut app, left);
        let right_window = window_for(&mut app, right);
        assert_eq!(count(&mut app), 2);

        disconnect(&mut app, right);
        assert!(app.world().get::<Window>(right_window).is_some());
        // Even if winit reattaches the old window to a surviving monitor, its
        // original assignment still determines whether it should be deleted.
        app.world_mut()
            .entity_mut(right_window)
            .insert(OnMonitor(left));
        app.world_mut().resource_mut::<desktop::Drag>().entity = Some(left_window);
        app.update();
        assert!(app.world().get_entity(right_window).is_err());
        assert_eq!(window_for(&mut app, left), left_window);
        assert_eq!(count(&mut app), 1);
        assert!(app.world().resource::<desktop::Drag>().entity.is_none());

        let reconnected = app.world_mut().spawn_empty().id();
        app.world_mut()
            .resource_mut::<LiveMonitors>()
            .0
            .push((reconnected, output("right")));
        app.update();
        let replacement = window_for(&mut app, reconnected);
        app.update();
        assert_eq!(window_for(&mut app, reconnected), replacement);
        assert_eq!(count(&mut app), 2);

        disconnect(&mut app, left);
        disconnect(&mut app, reconnected);
        app.update();
        assert_eq!(count(&mut app), 0);
        assert!(app.world().get_entity(left_window).is_err());
        assert!(app.world().get_entity(replacement).is_err());
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
    }

    #[test]
    fn output_sync_follows_monitor_changes() {
        #[derive(Resource, Default)]
        struct LiveMonitors(Vec<(Entity, Output)>);
        fn read_monitors(monitors: Res<LiveMonitors>) -> Vec<(Entity, Output)> {
            monitors.0.clone()
        }
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .init_resource::<LiveMonitors>()
            .add_systems(Update, read_monitors.pipe(outputs));
        let monitor = app
            .world_mut()
            .spawn(Monitor {
                name: Some("m".into()),
                physical_width: 1920,
                physical_height: 1080,
                physical_position: IVec2::ZERO,
                scale_factor: 1.,
                refresh_rate_millihertz: None,
                video_modes: Vec::new(),
            })
            .id();
        app.world_mut().resource_mut::<LiveMonitors>().0 = vec![(
            monitor,
            Output {
                name: "m".into(),
                origin: Vec2::ZERO,
                size: Vec2::new(1920., 1080.),
                units_per_logical_pixel: 1.,
            },
        )];
        app.update();
        let window = app.world().get::<HasWindows>(monitor).unwrap().collection()[0];
        let output = app.world().get::<Output>(window).unwrap();
        assert_eq!(output.name, "m");
        assert_eq!(output.origin, Vec2::ZERO);
        assert_eq!(output.size, Vec2::new(1920., 1080.));
        assert_eq!(output.units_per_logical_pixel, 1.);
        let current = app.world().get::<Window>(window).unwrap();
        assert_eq!(current.position, WindowPosition::At(IVec2::ZERO));
        assert_eq!(current.resolution.physical_width(), 1920);
        assert_eq!(current.resolution.scale_factor(), 1.);

        {
            let mut live = app.world_mut().resource_mut::<LiveMonitors>();
            let monitor = &mut live.0[0].1;
            monitor.name = "renamed".into();
            monitor.origin = Vec2::new(-2560., 0.);
            monitor.size.x = 2560.;
            monitor.units_per_logical_pixel = 2.;
        }
        app.update();
        assert_eq!(
            app.world().get::<HasWindows>(monitor).unwrap().collection(),
            &[window]
        );
        let output = app.world().get::<Output>(window).unwrap();
        assert_eq!(output.name, "renamed");
        assert_eq!(output.origin, Vec2::new(-2560., 0.));
        assert_eq!(output.size, Vec2::new(2560., 1080.));
        assert_eq!(output.units_per_logical_pixel, 2.);
        let current = app.world().get::<Window>(window).unwrap();
        assert_eq!(current.position, WindowPosition::At(IVec2::new(-2560, 0)));
        assert_eq!(current.resolution.physical_width(), 2560);
        assert_eq!(current.resolution.scale_factor(), 2.);
    }
}
