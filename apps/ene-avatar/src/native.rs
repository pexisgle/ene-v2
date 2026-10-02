use crate::mask::Run;
use ene_protocol::avatar::{AvatarEvent, AvatarSettings};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
};
use std::ptr::NonNull;
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_output, wl_pointer, wl_seat, wl_surface},
};

/// Keeps the libwayland display and layer role alive for the WGPU surface.
#[derive(Debug, Clone)]
pub struct NativeWindow {
    connection: Connection,
    layer: LayerSurface,
}
impl HasWindowHandle for NativeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        use wayland_client::Proxy;
        let ptr = NonNull::new(self.layer.wl_surface().id().as_ptr().cast())
            .ok_or(HandleError::Unavailable)?;
        // SAFETY: the owned layer retains its wl_surface throughout this borrow.
        Ok(unsafe {
            WindowHandle::borrow_raw(RawWindowHandle::Wayland(WaylandWindowHandle::new(ptr)))
        })
    }
}
impl HasDisplayHandle for NativeWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let ptr = NonNull::new(self.connection.backend().display_ptr().cast())
            .ok_or(HandleError::Unavailable)?;
        // SAFETY: the owned connection retains libwayland's display.
        Ok(unsafe {
            DisplayHandle::borrow_raw(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(ptr)))
        })
    }
}

use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

struct Drag {
    press: (f64, f64),
    latest: (f64, f64),
    margins: (i32, i32),
    delta: (f64, f64),
    relative_seen: bool,
}

pub struct State {
    connection: Connection,
    pub registry: RegistryState,
    pub seat: SeatState,
    pub outputs: OutputState,
    pub compositor: CompositorState,
    pub shell: LayerShell,
    pub layer: LayerSurface,
    pub settings: AvatarSettings,
    requested_layer_size: (u32, u32),
    pub configured: bool,
    pub closed: bool,
    pub scale: u32,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
    pub output_changed: bool,
    pointer: Option<wl_pointer::WlPointer>,
    drag: Option<Drag>,
    relative_state: RelativePointerState,
    relative_pointer: Option<ZwpRelativePointerV1>,
    drag_pending: bool,
    recenter_pending: bool,
    input_runs: Vec<Run>,
    surface_output: Option<wl_output::WlOutput>,
}
impl State {
    pub fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
        settings: AvatarSettings,
        connection: Connection,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let compositor = CompositorState::bind(globals, qh)?;
        let shell = LayerShell::bind(globals, qh).map_err(
            |_| "この環境はWayland layer-shellに対応していません（KDE/KWinなどの対応環境で起動してください）。",
        )?;
        let outputs = OutputState::new(globals, qh);
        let surface = compositor.create_surface(qh);
        let layer = shell.create_layer_surface(qh, surface, Layer::Top, Some("ene-avatar"), None);
        let mut state = Self {
            connection,
            registry: RegistryState::new(globals),
            seat: SeatState::new(globals, qh),
            outputs,
            compositor,
            shell,
            layer,
            settings,
            requested_layer_size: (0, 0),
            configured: false,
            closed: false,
            scale: 1,
            width: 1,
            height: 1,
            resized: false,
            output_changed: false,
            pointer: None,
            drag: None,
            relative_state: RelativePointerState::bind(globals, qh),
            relative_pointer: None,
            drag_pending: false,
            recenter_pending: false,
            input_runs: Vec::new(),
            surface_output: None,
        };
        state.properties();
        state.layer.commit();
        Ok(state)
    }
    pub fn properties(&mut self) {
        self.layer.set_anchor(Anchor::BOTTOM | Anchor::RIGHT);
        self.layer
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        self.layer.set_exclusive_zone(-1);
        let size = self.requested_size();
        let request_changed = self.requested_layer_size != size;
        self.requested_layer_size = size;
        self.layer.set_size(size.0, size.1);
        if self.configured && request_changed && (self.width, self.height) != size {
            // Await the compositor's new size before presenting a resized frame.
            self.configured = false;
            self.drag = None;
            self.drag_pending = false;
            // Retain the old buffer and its matching region until the resized
            // GPU frame is ready; pending layer size does not replace pixels.
        }
        self.layer
            .set_margin(0, self.settings.right, self.settings.bottom, 0);
    }
    pub fn native_window(&self) -> NativeWindow {
        NativeWindow {
            connection: self.connection.clone(),
            layer: self.layer.clone(),
        }
    }
    /// Set pending input and scale before WGPU's present performs the commit.
    pub fn prepare_gpu_present(&mut self, runs: &[Run]) {
        self.set_input(runs);
        self.input_runs.clear();
        self.input_runs.extend_from_slice(runs);
        self.layer.wl_surface().set_buffer_scale(self.scale as i32);
        self.layer.wl_surface().damage_buffer(
            0,
            0,
            (self.width * self.scale) as i32,
            (self.height * self.scale) as i32,
        );
    }
    fn output_size(&self) -> (i32, i32) {
        if let Some(output) = &self.surface_output
            && let Some(size) = self.outputs.info(output).and_then(|info| info.logical_size)
        {
            return size;
        }
        self.outputs
            .outputs()
            .find_map(|o| {
                self.outputs.info(&o).and_then(|i| {
                    if self
                        .settings
                        .output
                        .as_ref()
                        .is_none_or(|n| i.name.as_ref() == Some(n))
                    {
                        i.logical_size
                    } else {
                        None
                    }
                })
            })
            .unwrap_or((1920, 1080))
    }
    pub fn avatar_height(&self) -> f32 {
        self.output_size().1.max(1) as f32 * 0.4 * self.settings.size_percent as f32 / 100.
    }
    fn requested_size(&self) -> (u32, u32) {
        let output = self.output_size();
        let height = self.avatar_height() * 2.;
        // Keep room around the body for motions. Limiting the render target
        // does not limit the avatar's scale: the camera sets its pixel height.
        (
            (height * 4. / 3.)
                .min(output.0.max(1) as f32)
                .round()
                .max(1.) as u32,
            height.min(output.1.max(1) as f32).round().max(1.) as u32,
        )
    }
    pub fn request_recenter(&mut self) {
        self.recenter_pending = true;
        self.drag = None;
        self.drag_pending = false;
    }
    pub fn apply_recenter(&mut self) {
        if !self.recenter_pending || !self.configured {
            return;
        }
        self.recenter_pending = false;
        let size = self.output_size();
        self.settings.right = (size.0 - self.width as i32) / 2;
        self.settings.bottom = (size.1 - self.height as i32) / 2;
        self.layer
            .set_margin(0, self.settings.right, self.settings.bottom, 0);
        self.layer.commit();
        self.notify_position();
    }
    pub fn recreate(&mut self, qh: &QueueHandle<Self>) {
        let output = self.outputs.outputs().find(|o| {
            self.outputs.info(o).is_some_and(|i| {
                self.settings
                    .output
                    .as_ref()
                    .is_some_and(|n| i.name.as_ref() == Some(n))
            })
        });
        if let Some(ref o) = output {
            self.settings.output = self.outputs.info(o).and_then(|i| i.name);
        } else {
            self.settings.output = None;
        }
        let surface = self.compositor.create_surface(qh);
        self.layer = self.shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("ene-avatar"),
            output.as_ref(),
        );
        self.drag = None;
        self.drag_pending = false;
        self.requested_layer_size = (0, 0);
        self.configured = false;
        self.closed = false;
        self.scale = 1;
        self.resized = true;
        self.surface_output = None;
        self.properties();
        self.set_input(&[]);
        self.input_runs.clear();
        self.layer.commit();
    }
    pub fn apply_drag(&mut self) {
        if !self.drag_pending {
            return;
        }
        self.drag_pending = false;
        let Some(drag) = &self.drag else {
            return;
        };
        let delta = if drag.relative_seen {
            drag.delta
        } else {
            // Without relative motion keep the surface stationary until release,
            // so all absolute samples still have the press-time coordinate system.
            (drag.latest.0 - drag.press.0, drag.latest.1 - drag.press.1)
        };
        let desired = (
            (f64::from(drag.margins.0) - delta.0).round() as i32,
            (f64::from(drag.margins.1) - delta.1).round() as i32,
        );
        let previous = (self.settings.right, self.settings.bottom);
        self.settings.right = desired.0;
        self.settings.bottom = desired.1;
        if previous != (self.settings.right, self.settings.bottom) {
            self.layer
                .set_margin(0, self.settings.right, self.settings.bottom, 0);
            self.layer.commit();
        }
    }
    pub fn notify_position(&self) {
        crate::emit(AvatarEvent::PositionChanged {
            right: self.settings.right,
            bottom: self.settings.bottom,
            output: self.settings.output.clone(),
        });
    }
    fn set_input(&self, runs: &[Run]) {
        let region =
            smithay_client_toolkit::compositor::Region::new(&self.compositor).expect("wl_region");
        for (run, height) in input_rectangles(runs) {
            region.add(run.x, run.y, run.width, height);
        }
        self.layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));
    }
}
impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        factor: i32,
    ) {
        if surface == self.layer.wl_surface() {
            self.scale = factor.max(1) as u32;
            self.resized = true;
            // Apply the new scale atomically with the matching-size buffer in
            // present(), keeping the old displayed frame at its old scale.
        }
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        if surface == self.layer.wl_surface() {
            self.surface_output = Some(output.clone());
            self.settings.output = self.outputs.info(output).and_then(|i| i.name);
            self.properties();
            self.layer.commit();
            self.notify_position();
        }
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        if self.settings.visible {
            self.properties();
            self.layer.commit();
        }
    }
    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if self.surface_output.as_ref() == Some(&output) {
            self.output_changed = true;
        }
    }
}
impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer == &self.layer {
            self.closed = true;
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        c: LayerSurfaceConfigure,
        _: u32,
    ) {
        if layer != &self.layer {
            return;
        }
        let requested = self.requested_size();
        let width = if c.new_size.0 == 0 {
            requested.0
        } else {
            c.new_size.0
        };
        let height = if c.new_size.1 == 0 {
            requested.1
        } else {
            c.new_size.1
        };
        // Margin-only configure events must not clear the input region or
        // recreate the render target: this was a source of flicker while dragging.
        self.resized |= (self.width, self.height) != (width, height);
        self.width = width;
        self.height = height;
        self.configured = true;
    }
}
impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        c: Capability,
    ) {
        if c == Capability::Pointer && self.pointer.is_none() {
            match self.seat.get_pointer(qh, &seat) {
                Ok(pointer) => {
                    self.relative_pointer =
                        self.relative_state.get_relative_pointer(&pointer, qh).ok();
                    self.pointer = Some(pointer);
                }
                Err(error) => eprintln!("could not bind pointer: {error}"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        c: Capability,
    ) {
        if c == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            if let Some(relative) = self.relative_pointer.take() {
                relative.destroy();
            }
            p.release();
            self.drag = None;
            self.drag_pending = false;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        self.drag = None;
        self.drag_pending = false;
    }
}
impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Press { button: 0x110, .. } => {
                    // Also reject a stale compositor focus after an arm moves
                    // away underneath a stationary mouse cursor.
                    self.drag =
                        crate::mask::contains(&self.input_runs, event.position).then_some(Drag {
                            press: event.position,
                            latest: event.position,
                            margins: (self.settings.right, self.settings.bottom),
                            delta: (0.0, 0.0),
                            relative_seen: false,
                        });
                    self.drag_pending = false;
                }
                PointerEventKind::Motion { .. } => {
                    if let Some(drag) = &mut self.drag {
                        drag.latest = event.position;
                    }
                }
                PointerEventKind::Release { button: 0x110, .. } => {
                    self.drag_pending = self.drag.is_some();
                    self.apply_drag();
                    if let Some(drag) = self.drag.take() {
                        if (i64::from(self.settings.right) - i64::from(drag.margins.0)).abs()
                            + (i64::from(self.settings.bottom) - i64::from(drag.margins.1)).abs()
                            < 4
                        {
                            crate::emit(AvatarEvent::ShowChat);
                        } else {
                            crate::emit(AvatarEvent::PositionChanged {
                                right: self.settings.right,
                                bottom: self.settings.bottom,
                                output: self.settings.output.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
}
impl RelativePointerHandler for State {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpRelativePointerV1,
        pointer: &wl_pointer::WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.pointer.as_ref() != Some(pointer) {
            return;
        }
        if let Some(drag) = &mut self.drag {
            // Relative movement does not depend on the surface's current origin.
            // Accumulate subpixel motion, then commit once after dispatch.
            drag.delta.0 += event.delta.0;
            drag.delta.1 += event.delta.1;
            drag.relative_seen = true;
            self.drag_pending = true;
        }
    }
}
impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
smithay_client_toolkit::delegate_registry!(State);
smithay_client_toolkit::delegate_dispatch2!(State);

/// Combine only identical intervals on adjacent rows, preserving every hole.
fn input_rectangles(runs: &[Run]) -> Vec<(Run, i32)> {
    let mut rectangles: Vec<(Run, i32)> = Vec::new();
    let mut last = std::collections::BTreeMap::new();
    for &run in runs {
        let key = (run.x, run.width);
        if let Some(&index) = last.get(&key) {
            let (previous, height): &mut (Run, i32) = &mut rectangles[index];
            if previous.y + *height == run.y {
                *height += 1;
                continue;
            }
        }
        last.insert(key, rectangles.len());
        rectangles.push((run, 1));
    }
    rectangles
}
