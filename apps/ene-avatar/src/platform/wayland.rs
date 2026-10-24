use crate::{
    character::AvatarState,
    control::Control,
    desktop::{self, Drag, HitRegions, Output},
};
use bevy::{
    app::{AppExit, PluginsState},
    prelude::*,
    window::{RawHandleWrapper, WindowWrapper},
};
use exwlshellev::{
    id::Id,
    reexport::{
        wayland_client::{ButtonState, WEnum},
        *,
    },
    *,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub fn setup(app: &mut App) {
    app.set_runner(run);
}

fn pointer(app: &mut App, output: Entity, local: Vec2, button: Option<bool>) {
    app.world_mut().write_message(desktop::PointerInput {
        output,
        local,
        button,
    });
}

fn run(mut app: App) -> AppExit {
    let state: WindowState<()> = match WindowState::new("ene-avatar")
        .with_allscreens()
        .with_use_display_handle(true)
        .with_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right)
        .with_layer(Layer::Top)
        .with_exclusive_zone(-1)
        .with_keyboard_interacivity(KeyboardInteractivity::None)
        .build()
    {
        Ok(state) => state,
        Err(error) => {
            let _ = app.world().resource::<Control>().output.try_send(serde_json::json!({"event":"error","error":format!("Wayland layer-shell and xdg-output are required: {error}")}));
            return AppExit::error();
        }
    };
    while app.plugins_state() != PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
    }
    app.finish();
    app.cleanup();
    let mut windows = HashMap::<Id, Entity>::new();
    let mut compositor = None;
    let mut cursor = Vec2::ZERO;
    let mut deadline = Instant::now();
    let wake = app
        .world_mut()
        .remove_non_send::<exwlshellev::calloop::channel::Channel<()>>()
        .unwrap();
    let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel(1);
    let result = state.running_with_proxy(wake, move |event, state, id| {
        match event {
            ExWlShellEvent::InitRequest => return ReturnData::RequestCompositor,
            ExWlShellEvent::CompositorProvide(c, qh) => {
                compositor = Some((c.clone(), qh.clone()));
            }
            ExWlShellEvent::UserEvent(()) => {
                deadline = Instant::now();
            }
            ExWlShellEvent::RequestMessages(
                DispatchMessage::OutputAdded(_)
                | DispatchMessage::OutputUpdated(_)
                | DispatchMessage::OutputRemoved(_),
            ) => {
                deadline = Instant::now();
                state.request_refresh_all(RefreshRequest::NextFrame);
            }
            ExWlShellEvent::RequestMessages(DispatchMessage::Closed) => {
                if let Some(entity) = id.and_then(|id| windows.remove(&id)) {
                    app.world_mut().despawn(entity);
                }
                deadline = Instant::now();
            }
            ExWlShellEvent::RequestMessages(message) => {
                if let Some((id, unit)) =
                    id.and_then(|id| state.get_unit_with_id(id).map(|unit| (id, unit)))
                {
                    if !windows.contains_key(&id) && unit.get_size().0 > 0 && unit.get_size().1 > 0
                    {
                        let Some((name, origin)) = state.get_output_info(id).and_then(|info| {
                            Some((
                                info.name.filter(|name| !name.is_empty())?,
                                info.logical_position?,
                            ))
                        }) else {
                            return ReturnData::None;
                        };
                        let output = Output {
                            name,
                            origin: Vec2::new(origin.0 as f32, origin.1 as f32),
                            size: Vec2::new(unit.get_size().0 as f32, unit.get_size().1 as f32),
                            units_per_logical_pixel: 1.,
                        };
                        let wrapper = WindowWrapper::new(unit.gen_wrapper());
                        let handle =
                            RawHandleWrapper::new(&wrapper).expect("valid Wayland surface handle");
                        let window = app
                            .world_mut()
                            .spawn((desktop::window(&output), handle, output))
                            .id();
                        windows.insert(id, window);
                    }
                    if let Some(&window) = windows.get(&id) {
                        match message {
                            DispatchMessage::MouseEnter {
                                surface_x,
                                surface_y,
                                ..
                            }
                            | DispatchMessage::MouseMotion {
                                surface_x,
                                surface_y,
                                ..
                            } => {
                                cursor = Vec2::new(*surface_x as f32, *surface_y as f32);
                                pointer(&mut app, window, cursor, None);
                            }
                            DispatchMessage::MouseButton {
                                button: 0x110,
                                state,
                                ..
                            } => {
                                pointer(
                                    &mut app,
                                    window,
                                    cursor,
                                    Some(*state == WEnum::Value(ButtonState::Pressed)),
                                );
                            }
                            DispatchMessage::Unfocus => {
                                app.world_mut().resource_mut::<Drag>().entity = None;
                            }
                            _ => {}
                        }
                    }
                }
            }
            ExWlShellEvent::NormalDispatch => {
                if state.get_pointers_iter().next().is_none() {
                    app.world_mut().resource_mut::<Drag>().entity = None;
                }
                if Instant::now() >= deadline {
                    let started = Instant::now();
                    for (&id, &window) in &windows {
                        if let Some(unit) = state.get_unit_with_id(id) {
                            let Some((x, y)) = state
                                .get_output_info(id)
                                .and_then(|info| info.logical_position)
                            else {
                                continue;
                            };
                            let (width, height) = unit.get_size();
                            let scale = if unit
                                .try_set_viewport_destination(width as i32, height as i32)
                                .is_some()
                            {
                                unit.scale_float() as f32
                            } else {
                                unit.get_wlsurface()
                                    .set_buffer_scale(unit.scale_float().ceil() as i32);
                                unit.scale_float().ceil() as f32
                            };
                            let size = Vec2::new(width as f32, height as f32);
                            let position = Vec2::new(x as f32, y as f32);
                            if let Some(mut output) = app.world_mut().get_mut::<Output>(window)
                                && (output.origin != position || output.size != size)
                            {
                                output.origin = position;
                                output.size = size;
                            }
                            let mut window = app.world_mut().get_mut::<Window>(window).unwrap();
                            window.resolution.set_scale_factor(scale);
                            window.resolution.set(width as f32, height as f32);
                        }
                    }
                    app.update();
                    if windows.is_empty() && {
                        let mut query = app.world_mut().query::<&AvatarState>();
                        query
                            .iter(app.world())
                            .any(|state| *state == AvatarState::Loading)
                    } {
                        // Without surfaces, only unfinished scene loading needs another frame.
                        let _ = app.world().resource::<Control>().wake_tx.send(());
                    }
                    if let Some(exit) = app.should_exit() {
                        let _ = exit_tx.try_send(exit);
                        return ReturnData::RequestExit;
                    }
                    if let Some((compositor, qh)) = &compositor {
                        for (&id, &window) in &windows {
                            if let Some(unit) = state.get_unit_with_id(id) {
                                let region = compositor.create_region(qh, ());
                                if let Some(regions) = app.world().get::<HitRegions>(window) {
                                    for (_, r) in &regions.0 {
                                        let min = r.min.floor().as_ivec2();
                                        let max = r.max.ceil().as_ivec2();
                                        region.add(min.x, min.y, max.x - min.x, max.y - min.y);
                                    }
                                }
                                unit.get_wlsurface().set_input_region(Some(&region));
                                region.destroy();
                                unit.get_wlsurface().commit();
                            }
                        }
                    }
                    let fps = app.world().resource::<Control>().fps;
                    deadline = started
                        + if fps == 0 {
                            Duration::ZERO
                        } else {
                            Duration::from_secs_f64(1. / f64::from(fps))
                        };
                    state.request_refresh_all(RefreshRequest::At(deadline));
                }
            }
            _ => {}
        }
        ReturnData::None
    });
    if let Err(error) = result {
        eprintln!("Wayland: {error}");
        return AppExit::error();
    }
    exit_rx.try_recv().unwrap_or(AppExit::Success)
}
