use super::{windows_input::input, windows_outputs::outputs};
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
    ecs::system::NonSendMarker,
    prelude::*,
    window::{CursorOptions, Monitor, RawHandleWrapper},
    winit::{UpdateMode, WinitMonitors, WinitSettings},
};
use std::time::Duration;

pub fn setup(app: &mut App) {
    app.add_systems(
        PreUpdate,
        (read_monitors.pipe(outputs), cursor_position.pipe(input))
            .chain()
            .before(desktop::pointer),
    )
    .add_systems(PreUpdate, cancel_lost_capture.after(desktop::pointer))
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
        _ => UpdateMode::Reactive {
            wait: Duration::from_secs_f64(1. / f64::from(fps)),
            react_to_device_events: false,
            react_to_user_events: false,
            react_to_window_events: false,
        },
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

fn cancel_lost_capture(handles: Query<&RawHandleWrapper, With<Output>>, mut drag: ResMut<Drag>) {
    if drag.entity.is_none() {
        return;
    }
    let mut info = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    let captured = handles.iter().any(|handle| {
        let raw_window_handle::RawWindowHandle::Win32(handle) = handle.get_window_handle() else {
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
