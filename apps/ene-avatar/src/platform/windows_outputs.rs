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
    use bevy::window::HasWindows;

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

        disconnect(&mut app, left);
        app.update();
        assert_eq!(count(&mut app), 0);
        assert!(app.world().get_entity(left_window).is_err());
        assert!(app.world().resource::<Time<Virtual>>().is_paused());
    }
}
