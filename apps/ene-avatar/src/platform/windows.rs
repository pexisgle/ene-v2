use crate::{
    control,
    desktop::{self, Drag, HitRegions, Output},
};
use ::windows::Win32::{
    Foundation::{HWND, POINT},
    UI::WindowsAndMessaging::*,
};
use bevy::{
    camera::visibility::VisibilitySystems,
    ecs::relationship::RelationshipTarget,
    ecs::system::NonSendMarker,
    input::{ButtonState, mouse::MouseButtonInput},
    prelude::*,
    window::{
        CursorOptions, HasWindows, Monitor, OnMonitor, RawHandleWrapper, WindowLevel,
        WindowPosition,
    },
    winit::{UpdateMode, WinitMonitors, WinitSettings},
};
use std::time::Duration;

pub fn setup(app: &mut App) {
    app.add_systems(
        PreUpdate,
        (read_monitors.pipe(outputs), input)
            .chain()
            .before(desktop::pointer),
    )
    .add_systems(
        Update,
        sync_update_mode.run_if(resource_changed::<control::Control>),
    )
    .add_systems(
        PostUpdate,
        hit_test.after(VisibilitySystems::CheckVisibility),
    )
    .add_systems(control::FrameEnd, window_styles);
}

fn update_mode(fps: u32) -> UpdateMode {
    match fps {
        0 => UpdateMode::Continuous,
        _ => UpdateMode::reactive_low_power(Duration::from_secs_f64(1. / f64::from(fps))),
    }
}

fn sync_update_mode(control: Res<control::Control>, mut settings: ResMut<WinitSettings>) {
    let mode = update_mode(control.fps);
    settings.focused_mode = mode;
    settings.unfocused_mode = mode;
}

fn read_monitors(
    native: Res<WinitMonitors>,
    monitors: Query<Entity, With<Monitor>>,
) -> Vec<(Entity, Output)> {
    monitors
        .iter()
        .filter_map(|entity| {
            // Bevy's Monitor component is a creation-time snapshot.
            let monitor = native.find_entity(entity)?;
            let position = monitor.position();
            let size = monitor.size();
            Some((
                entity,
                Output {
                    name: monitor
                        .name()
                        .unwrap_or_else(|| format!("monitor-{}", entity.to_bits())),
                    origin: Vec2::new(position.x as f32, position.y as f32),
                    size: Vec2::new(size.width as f32, size.height as f32),
                    units_per_logical_pixel: monitor.scale_factor() as f32,
                },
            ))
        })
        .collect()
}

fn outputs(
    In(snapshots): In<Vec<(Entity, Output)>>,
    monitors: Query<&HasWindows>,
    mut outputs: Query<(&mut Output, &mut Window)>,
    mut commands: Commands,
) {
    for (monitor, output) in snapshots {
        let Ok(windows) = monitors.get(monitor) else {
            continue;
        };
        let old_window = windows.collection().first().copied();
        if let Some(window) = old_window
            && let Ok((mut previous, mut window)) = outputs.get_mut(window)
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
            output,
        ));
    }
}

fn hit_test(drag: Res<Drag>, mut windows: Query<(&Output, &HitRegions, &mut CursorOptions)>) {
    let Some(point) = cursor_position() else {
        return;
    };
    for (output, regions, mut cursor) in &mut windows {
        let local = (point - output.origin) / output.units_per_logical_pixel;
        // shortcut: input switching follows rendering FPS; keep it here unless measured lag is unacceptable.
        let hit = drag.entity.is_some() || regions.0.iter().any(|(_, r)| r.contains(local));
        if cursor.hit_test != hit {
            cursor.hit_test = hit;
        }
    }
}

fn cursor_position() -> Option<Vec2> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some(Vec2::new(point.x as f32, point.y as f32))
}

// Winit rewrites styles in Last, so restore non-activation afterwards.
fn window_styles(windows: Query<&RawHandleWrapper, With<Output>>, _main_thread: NonSendMarker) {
    for handle in &windows {
        if let raw_window_handle::RawWindowHandle::Win32(handle) = handle.get_window_handle() {
            let hwnd = HWND(handle.hwnd.get() as *mut _);
            unsafe {
                let flags = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                let needed = flags | WS_EX_NOACTIVATE.0;
                if needed != flags {
                    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, needed as isize);
                }
            }
        }
    }
}

fn input(
    mut buttons: MessageReader<MouseButtonInput>,
    outputs: Query<(Entity, &Output)>,
    handles: Query<&RawHandleWrapper, With<Output>>,
    mut drag: ResMut<Drag>,
    mut pointer: MessageWriter<desktop::PointerInput>,
) {
    let Some(point) = cursor_position() else {
        return;
    };
    let output = outputs.iter().find_map(|(entity, output)| {
        desktop::bounds(output).contains(point).then(|| {
            (
                entity,
                (point - output.origin) / output.units_per_logical_pixel,
            )
        })
    });
    let mut released = false;
    if let Some((output, local)) = output {
        pointer.write(desktop::PointerInput {
            output,
            local,
            button: None,
        });
        for event in buttons
            .read()
            .filter(|event| event.button == MouseButton::Left)
        {
            released |= event.state == ButtonState::Released;
            pointer.write(desktop::PointerInput {
                output,
                local,
                button: Some(event.state == ButtonState::Pressed),
            });
        }
    } else {
        released = buttons
            .read()
            .any(|event| event.button == MouseButton::Left && event.state == ButtonState::Released);
    }
    if drag.entity.is_some() && !released {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let captured = handles.iter().any(|handle| {
            let raw_window_handle::RawWindowHandle::Win32(handle) = handle.get_window_handle()
            else {
                return false;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut _);
            unsafe {
                let thread = GetWindowThreadProcessId(hwnd, None);
                GetGUIThreadInfo(thread, &mut info).is_ok() && info.hwndCapture == hwnd
            }
        });
        if !captured {
            drag.entity = None;
        }
    }
    if released && output.is_none() {
        drag.entity = None;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

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
