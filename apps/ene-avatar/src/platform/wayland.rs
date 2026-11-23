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
        wayland_client::{ButtonState, QueueHandle, WEnum, WlCompositor},
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

/// A dispatch step that answered the callback early, with the reply to return.
/// `Some` skips the frame body and the trailing refresh request, as the
/// `return` it replaced did.
type Step = Option<ReturnData<()>>;

fn run(mut app: App) -> AppExit {
    let state = match layer_state(&mut app) {
        Ok(state) => state,
        Err(exit) => return exit,
    };
    start_plugins(&mut app);
    let Some(wake) = app
        .world_mut()
        .remove_non_send::<exwlshellev::calloop::channel::Channel<()>>()
    else {
        return AppExit::error();
    };
    let (exit_tx, exit_rx) = std::sync::mpsc::sync_channel(1);
    let mut renderer = Renderer {
        app,
        windows: HashMap::new(),
        surface_sizes: HashMap::new(),
        surface_regions: HashMap::new(),
        next_region: Vec::new(),
        compositor: None,
        cursor: Vec2::ZERO,
        deadline: Instant::now(),
        exit: exit_tx,
    };
    let result = state.running_with_proxy(wake, move |event, state, id| {
        renderer.handle(event, state, id)
    });
    if let Err(error) = result {
        bevy::log::error!(%error, "wayland loop failed");
        return AppExit::error();
    }
    exit_rx.try_recv().unwrap_or(AppExit::Success)
}

/// Builds the layer-shell surface the avatar draws into, reporting a compositor
/// that lacks layer-shell or xdg-output through the control channel.
fn layer_state(app: &mut App) -> Result<WindowState<()>, AppExit> {
    match WindowState::new("ene-avatar")
        .with_allscreens()
        .with_use_display_handle(true)
        .with_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right)
        .with_layer(Layer::Top)
        .with_exclusive_zone(-1)
        .with_keyboard_interacivity(KeyboardInteractivity::None)
        .build()
    {
        Ok(state) => Ok(state),
        Err(error) => {
            let _reported = app.world().resource::<Control>().output.try_send(serde_json::json!({"event":"error","error":format!("Wayland layer-shell and xdg-output are required: {error}")}));
            Err(AppExit::error())
        }
    }
}

/// Bevy finishes plugin startup on the calling thread before the runner owns it.
fn start_plugins(app: &mut App) {
    while app.plugins_state() != PluginsState::Ready {
        bevy::tasks::tick_global_task_pools_on_main_thread();
    }
    app.finish();
    app.cleanup();
}

/// Everything one renderer run mutates between dispatches.
///
/// The event callback borrows this for its whole life, so the surfaces, the
/// frame clock and the Bevy app live in one place instead of being threaded
/// through the dispatch as separate `&mut` parameters.
struct Renderer {
    app: App,
    windows: HashMap<Id, Entity>,
    /// Last `(width, height, scale)` pushed to each surface, so an unchanged
    /// surface costs one comparison.
    surface_sizes: HashMap<Id, (u32, u32, f64)>,
    /// Last integer pixel region sent to the compositor per surface.
    surface_regions: HashMap<Id, Vec<(IVec2, IVec2)>>,
    /// Reused per-frame buffer for the region a surface should have now.
    next_region: Vec<(IVec2, IVec2)>,
    compositor: Option<(WlCompositor, QueueHandle<WindowState<()>>)>,
    cursor: Vec2,
    deadline: Instant,
    exit: std::sync::mpsc::SyncSender<AppExit>,
}

impl Renderer {
    fn handle(
        &mut self,
        event: ExWlShellEvent<(), ()>,
        state: &mut WindowState<()>,
        id: Option<Id>,
    ) -> ReturnData<()> {
        if let Some(reply) = self.dispatch(event, state, id) {
            return reply;
        }
        if let Some(exit) = self.frame(state) {
            let _exited = self.exit.try_send(exit);
            return ReturnData::RequestExit;
        }
        // RequestRefresh consumes its timer even when this frame is not due yet.
        state.request_refresh_all(RefreshRequest::At(self.deadline));
        ReturnData::None
    }

    fn dispatch(
        &mut self,
        event: ExWlShellEvent<(), ()>,
        state: &mut WindowState<()>,
        id: Option<Id>,
    ) -> Step {
        match event {
            ExWlShellEvent::InitRequest => Some(ReturnData::RequestCompositor),
            ExWlShellEvent::CompositorProvide(c, qh) => {
                self.compositor = Some((c.clone(), qh.clone()));
                // This reply ends exwlshellev's init loop, and that loop only
                // accepts `None`, `RequestBind` or `RequestCompositor`: any
                // other value panics. Returning `None` here skips the frame
                // body below, so a quit that arrives before the first dispatch
                // cannot raise `RequestExit` inside the handshake. Dispatch
                // handles it one event later.
                Some(ReturnData::None)
            }
            ExWlShellEvent::UserEvent(()) => {
                self.deadline = Instant::now();
                None
            }
            ExWlShellEvent::RequestMessages(
                DispatchMessage::OutputAdded(_)
                | DispatchMessage::OutputUpdated(_)
                | DispatchMessage::OutputRemoved(_),
            ) => {
                self.deadline = Instant::now();
                state.request_refresh_all(RefreshRequest::NextFrame);
                None
            }
            ExWlShellEvent::RequestMessages(DispatchMessage::Closed) => {
                if let Some(entity) = id.and_then(|id| self.windows.remove(&id)) {
                    self.app.world_mut().despawn(entity);
                    if let Some(id) = id {
                        self.surface_sizes.remove(&id);
                        self.surface_regions.remove(&id);
                    }
                }
                self.deadline = Instant::now();
                None
            }
            ExWlShellEvent::RequestMessages(message) => self.message(state, id, message),
            ExWlShellEvent::NormalDispatch if state.get_pointers_iter().next().is_none() => {
                desktop::cancel_pointer(self.app.world_mut());
                None
            }
            _ => None,
        }
    }

    fn message(
        &mut self,
        state: &WindowState<()>,
        id: Option<Id>,
        message: &DispatchMessage,
    ) -> Step {
        let (id, unit) = id.and_then(|id| state.get_unit_with_id(id).map(|unit| (id, unit)))?;
        if let Some(reply) = self.open_surface(state, id, unit) {
            return Some(reply);
        }
        if let Some(&window) = self.windows.get(&id) {
            self.route_input(state, window, message);
        }
        None
    }

    /// Spawns the Bevy window for a surface once the compositor has given it a
    /// size and an output to sit on.
    fn open_surface(
        &mut self,
        state: &WindowState<()>,
        id: Id,
        unit: &WindowStateUnit<()>,
    ) -> Step {
        let (width, height) = unit.get_size();
        if self.windows.contains_key(&id) || width == 0 || height == 0 {
            return None;
        }
        let Some((name, origin)) = state.get_output_info(id).and_then(|info| {
            Some((
                info.name.filter(|name| !name.is_empty())?,
                info.logical_position?,
            ))
        }) else {
            return Some(ReturnData::None);
        };
        let output = Output {
            name,
            origin: Vec2::new(origin.0 as f32, origin.1 as f32),
            size: Vec2::new(width as f32, height as f32),
            units_per_logical_pixel: 1.,
        };
        let wrapper = WindowWrapper::new(unit.gen_wrapper());
        let Ok(handle) = RawHandleWrapper::new(&wrapper) else {
            return Some(ReturnData::None);
        };
        let window = self
            .app
            .world_mut()
            .spawn((desktop::window(&output), handle, output))
            .id();
        self.windows.insert(id, window);
        None
    }

    fn route_input(&mut self, state: &WindowState<()>, window: Entity, message: &DispatchMessage) {
        let available = state.get_pointers_iter().next().is_some();
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
                self.cursor = Vec2::new(*surface_x as f32, *surface_y as f32);
                pointer(
                    &mut self.app,
                    window,
                    self.cursor,
                    PointerAction::Move { delta: Vec2::ZERO },
                    available,
                );
            }
            DispatchMessage::MouseButton {
                button: 0x110,
                state: button_state,
                ..
            } => {
                let action = if *button_state == WEnum::Value(ButtonState::Pressed) {
                    PointerAction::Press(PointerButton::Primary)
                } else {
                    PointerAction::Release(PointerButton::Primary)
                };
                pointer(&mut self.app, window, self.cursor, action, available);
            }
            DispatchMessage::Unfocus => desktop::cancel_pointer(self.app.world_mut()),
            _ => {}
        }
    }

    /// Runs one due frame. `None` means this dispatch arrived early, so nothing
    /// simulated and the deadline stands.
    fn frame(&mut self, state: &WindowState<()>) -> Option<AppExit> {
        if Instant::now() < self.deadline {
            return None;
        }
        let started = Instant::now();
        self.sync_outputs(state);
        self.app.update();
        self.wake_if_loading();
        if let Some(exit) = self.app.should_exit() {
            return Some(exit);
        }
        self.commit_regions(state);
        let fps = self.app.world().resource::<Control>().fps;
        self.deadline = started + frame_period(fps);
        None
    }

    /// Applies the compositor's geometry to every surface's window and output.
    fn sync_outputs(&mut self, state: &WindowState<()>) {
        for (&id, &window) in &self.windows {
            let Some(unit) = state.get_unit_with_id(id) else {
                continue;
            };
            let Some((x, y)) = state
                .get_output_info(id)
                .and_then(|info| info.logical_position)
            else {
                continue;
            };
            let (width, height) = unit.get_size();
            let size_key = (width, height, unit.scale_float());
            if self.surface_sizes.get(&id) != Some(&size_key) {
                let scale = surface_scale(unit, width, height);
                if let Some(mut window) = self.app.world_mut().get_mut::<Window>(window) {
                    window.resolution.set_scale_factor(scale);
                    window.resolution.set(width as f32, height as f32);
                }
                self.surface_sizes.insert(id, size_key);
                // Also commit geometry changes on an empty, inactive surface.
                self.surface_regions.remove(&id);
            }
            place_output(
                &mut self.app,
                window,
                Vec2::new(x as f32, y as f32),
                Vec2::new(width as f32, height as f32),
            );
        }
    }

    /// Without surfaces, only unfinished scene loading needs another frame.
    fn wake_if_loading(&mut self) {
        if !self.windows.is_empty() {
            return;
        }
        let mut query = self.app.world_mut().query::<&Character>();
        if query
            .iter(self.app.world())
            .any(|character| character.state == AvatarState::Loading)
        {
            let _woke = self.app.world().resource::<Control>().wake_tx.send(());
        }
    }

    /// Pushes each surface's hit regions to the compositor as its input region.
    fn commit_regions(&mut self, state: &WindowState<()>) {
        let Some((compositor, qh)) = &self.compositor else {
            return;
        };
        for (&id, &window) in &self.windows {
            let Some(unit) = state.get_unit_with_id(id) else {
                continue;
            };
            self.next_region.clear();
            if let Some(regions) = self.app.world().get::<HitRegions>(window) {
                self.next_region.extend(
                    regions
                        .0
                        .iter()
                        .map(|(_, r)| (r.min.floor().as_ivec2(), r.max.ceil().as_ivec2())),
                );
            }
            // Surface input regions are persistent. Only send a new
            // one when its integer pixel coverage changes.
            if self.surface_regions.get(&id) != Some(&self.next_region) {
                let region = compositor.create_region(qh, ());
                for &(min, max) in &self.next_region {
                    region.add(min.x, min.y, max.x - min.x, max.y - min.y);
                }
                unit.get_wlsurface().set_input_region(Some(&region));
                region.destroy();
                unit.get_wlsurface().commit();
                std::mem::swap(
                    self.surface_regions.entry(id).or_default(),
                    &mut self.next_region,
                );
            }
        }
    }
}

/// Destination scale for a surface. A viewport consumes the size, otherwise the
/// buffer scale absorbs it, so the renderer draws at the right density.
fn surface_scale(unit: &WindowStateUnit<()>, width: u32, height: u32) -> f32 {
    if unit
        .try_set_viewport_destination(width as i32, height as i32)
        .is_some()
    {
        return unit.scale_float() as f32;
    }
    unit.get_wlsurface()
        .set_buffer_scale(unit.scale_float().ceil() as i32);
    unit.scale_float().ceil() as f32
}

/// Mirrors compositor placement into the `Output` component the cameras read.
fn place_output(app: &mut App, window: Entity, origin: Vec2, size: Vec2) {
    if let Some(mut output) = app.world_mut().get_mut::<Output>(window)
        && (output.origin != origin || output.size != size)
    {
        output.origin = origin;
        output.size = size;
    }
}

/// Frame period the control channel asks for. Zero means render as fast as the
/// compositor accepts.
fn frame_period(fps: u32) -> Duration {
    if fps == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(1. / f64::from(fps))
    }
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
