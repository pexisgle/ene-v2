use crate::{
    character::{AvatarState, Character},
    control::Control,
    desktop::{self, HitRegions, NativeInput, Output},
};
use bevy::{
    app::{AppExit, PluginsState},
    picking::pointer::{PointerAction, PointerButton},
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

fn pointer(app: &mut App, output: Entity, local: Vec2, action: PointerAction, available: bool) {
    // Capability removal is visible before this dispatch replays its synthetic release.
    if !available {
        desktop::cancel_pointer(app.world_mut());
        return;
    }
    app.world_mut()
        .resource_mut::<NativeInput>()
        .send(output, local, action);
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
            let _reported = app.world().resource::<Control>().output.try_send(serde_json::json!({"event":"error","error":format!("Wayland layer-shell and xdg-output are required: {error}")}));
            return AppExit::error();
        }
    };
    while app.plugins_state() != PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
    }
    app.finish();
    app.cleanup();
    let mut windows = HashMap::<Id, Entity>::new();
    let mut surface_sizes = HashMap::new();
    let mut surface_regions = HashMap::<Id, Vec<(IVec2, IVec2)>>::new();
    let mut next_region = Vec::new();
    let mut compositor = None;
    let mut cursor = Vec2::ZERO;
    let mut deadline = Instant::now();
    let Some(wake) = app
        .world_mut()
        .remove_non_send::<exwlshellev::calloop::channel::Channel<()>>()
    else {
        return AppExit::error();
    };
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
                    if let Some(id) = id {
                        surface_sizes.remove(&id);
                        surface_regions.remove(&id);
                    }
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
                        let Ok(handle) = RawHandleWrapper::new(&wrapper) else {
                            return ReturnData::None;
                        };
                        let window = app
                            .world_mut()
                            .spawn((desktop::window(&output), handle, output))
                            .id();
                        windows.insert(id, window);
                    }
                    if let Some(&window) = windows.get(&id) {
                        let pointer_available = state.get_pointers_iter().next().is_some();
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
                                pointer(
                                    &mut app,
                                    window,
                                    cursor,
                                    PointerAction::Move { delta: Vec2::ZERO },
                                    pointer_available,
                                );
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
                                    if *state == WEnum::Value(ButtonState::Pressed) {
                                        PointerAction::Press(PointerButton::Primary)
                                    } else {
                                        PointerAction::Release(PointerButton::Primary)
                                    },
                                    pointer_available,
                                );
                            }
                            DispatchMessage::Unfocus => {
                                desktop::cancel_pointer(app.world_mut());
                            }
                            _ => {}
                        }
                    }
                }
            }
            ExWlShellEvent::NormalDispatch if state.get_pointers_iter().next().is_none() => {
                desktop::cancel_pointer(app.world_mut());
            }
            _ => {}
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
                    let size_key = (width, height, unit.scale_float());
                    if surface_sizes.get(&id) != Some(&size_key) {
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
                        if let Some(mut window) = app.world_mut().get_mut::<Window>(window) {
                            window.resolution.set_scale_factor(scale);
                            window.resolution.set(width as f32, height as f32);
                        }
                        surface_sizes.insert(id, size_key);
                        // Also commit geometry changes on an empty, inactive surface.
                        surface_regions.remove(&id);
                    }
                    let size = Vec2::new(width as f32, height as f32);
                    let position = Vec2::new(x as f32, y as f32);
                    if let Some(mut output) = app.world_mut().get_mut::<Output>(window)
                        && (output.origin != position || output.size != size)
                    {
                        output.origin = position;
                        output.size = size;
                    }
                }
            }
            app.update();
            if windows.is_empty() && {
                let mut query = app.world_mut().query::<&Character>();
                query
                    .iter(app.world())
                    .any(|character| character.state == AvatarState::Loading)
            } {
                // Without surfaces, only unfinished scene loading needs another frame.
                let _woke = app.world().resource::<Control>().wake_tx.send(());
            }
            if let Some(exit) = app.should_exit() {
                let _exited = exit_tx.try_send(exit);
                return ReturnData::RequestExit;
            }
            if let Some((compositor, qh)) = &compositor {
                for (&id, &window) in &windows {
                    if let Some(unit) = state.get_unit_with_id(id) {
                        next_region.clear();
                        if let Some(regions) = app.world().get::<HitRegions>(window) {
                            next_region.extend(
                                regions.0.iter().map(|(_, r)| {
                                    (r.min.floor().as_ivec2(), r.max.ceil().as_ivec2())
                                }),
                            );
                        }
                        // Surface input regions are persistent. Only send a new
                        // one when its integer pixel coverage changes.
                        if surface_regions.get(&id) != Some(&next_region) {
                            let region = compositor.create_region(qh, ());
                            for &(min, max) in &next_region {
                                region.add(min.x, min.y, max.x - min.x, max.y - min.y);
                            }
                            unit.get_wlsurface().set_input_region(Some(&region));
                            region.destroy();
                            unit.get_wlsurface().commit();
                            std::mem::swap(
                                surface_regions.entry(id).or_default(),
                                &mut next_region,
                            );
                        }
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
        }
        // RequestRefresh consumes its timer even when this frame is not due yet.
        state.request_refresh_all(RefreshRequest::At(deadline));
        ReturnData::None
    });
    if let Err(error) = result {
        bevy::log::error!(%error, "wayland loop failed");
        return AppExit::error();
    }
    exit_rx.try_recv().unwrap_or(AppExit::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::character::{Config, Notification};
    use bevy::picking::{
        hover::PointerCaptureMap,
        pointer::{PointerId, PointerMap, PointerPressState},
    };

    const PRESS: PointerAction = PointerAction::Press(PointerButton::Primary);
    const RELEASE: PointerAction = PointerAction::Release(PointerButton::Primary);

    fn interaction_app() -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, desktop::setup))
            .add_message::<Notification>();
        let output = app
            .world_mut()
            .spawn(Output {
                name: "test".into(),
                origin: Vec2::ZERO,
                size: Vec2::splat(100.),
                units_per_logical_pixel: 1.,
            })
            .id();
        let mut character = Character::new(
            "a".into(),
            Config {
                position: Some([50., 50.]),
                ..default()
            },
        );
        character.state = AvatarState::Ready;
        let actor = app
            .world_mut()
            .spawn((character, Transform::default()))
            .id();
        app.world_mut().spawn((
            Mesh3d::default(),
            bevy::camera::primitives::Aabb::from_min_max(Vec3::splat(-48.), Vec3::splat(48.)),
            GlobalTransform::from_translation(Vec3::new(50., -50., 0.)),
            InheritedVisibility::VISIBLE,
            ChildOf(actor),
        ));
        app.world_mut().get_mut::<HitRegions>(output).unwrap().0 =
            vec![(actor, Rect::from_corners(Vec2::ZERO, Vec2::splat(100.)))];
        (app, output, actor)
    }

    fn notifications(app: &mut App) -> Vec<serde_json::Value> {
        app.world_mut()
            .resource_mut::<Messages<Notification>>()
            .drain()
            .map(|notification| notification.0)
            .collect()
    }

    fn assert_released(app: &App) {
        assert!(
            !app.world()
                .resource::<PointerCaptureMap>()
                .is_captured(&PointerId::Mouse)
        );
        let pointer = app
            .world()
            .resource::<PointerMap>()
            .get_entity(PointerId::Mouse)
            .unwrap();
        assert!(
            !app.world()
                .get::<PointerPressState>(pointer)
                .unwrap()
                .is_any_pressed()
        );
    }

    #[test]
    fn removed_pointer_batch_cancels_before_synthetic_release_and_restores() {
        for queued_press in [false, true] {
            let (mut app, output, actor) = interaction_app();
            pointer(&mut app, output, Vec2::splat(50.), PRESS, true);
            if !queued_press {
                app.update();
            }
            pointer(&mut app, output, Vec2::splat(50.), RELEASE, false);
            desktop::cancel_pointer(app.world_mut());
            app.update();
            assert!(notifications(&mut app).is_empty());
            assert_released(&app);

            pointer(
                &mut app,
                output,
                Vec2::splat(80.),
                PointerAction::Move { delta: Vec2::ZERO },
                true,
            );
            pointer(&mut app, output, Vec2::splat(80.), RELEASE, true);
            app.update();
            assert!(notifications(&mut app).is_empty());
            assert_eq!(
                app.world().get::<Character>(actor).unwrap().config.position,
                Some([50., 50.])
            );
            assert_released(&app);

            for action in [PRESS, RELEASE] {
                pointer(&mut app, output, Vec2::splat(50.), action, true);
            }
            app.update();
            assert_eq!(
                notifications(&mut app),
                vec![serde_json::json!({"event":"click", "character":"a"})]
            );
            assert_released(&app);
        }
    }

    #[test]
    fn removed_pointer_preserves_earlier_queued_completed_gestures() {
        let (mut app, output, _) = interaction_app();
        for action in [PRESS, RELEASE, PRESS] {
            pointer(&mut app, output, Vec2::splat(50.), action, true);
        }
        pointer(&mut app, output, Vec2::splat(50.), RELEASE, false);
        desktop::cancel_pointer(app.world_mut());
        app.update();
        assert_eq!(
            notifications(&mut app),
            vec![serde_json::json!({"event":"click", "character":"a"})]
        );
        assert_released(&app);
    }
}
