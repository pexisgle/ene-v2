//! Wayland layer-shell state, layout and input regions.
mod output;
mod pointer;
mod window;

use crate::render::mask::Run;
use ene_ipc::renderer::{AvatarConfig, AvatarId, AvatarPlacement};
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
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_output, wl_pointer, wl_seat, wl_surface},
};

pub use window::NativeWindow;
use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

struct Drag {
    press: (f64, f64),
    latest: (f64, f64),
    margins: (i32, i32),
    delta: (f64, f64),
    relative_seen: bool,
}

pub struct State {
    pub id: AvatarId,
    pub output: crate::ipc::Output,
    pub requests: Vec<crate::ipc::WindowRequest>,
    pub stopping: bool,
    connection: Connection,
    pub registry: RegistryState,
    pub seat: SeatState,
    pub outputs: OutputState,
    pub compositor: CompositorState,
    pub shell: LayerShell,
    pub layer: LayerSurface,
    pub settings: AvatarConfig,
    pub placement: AvatarPlacement,
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
        id: AvatarId,
        settings: AvatarConfig,
        placement: AvatarPlacement,
        connection: Connection,
        output: crate::ipc::Output,
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
            id,
            output,
            requests: Vec::new(),
            stopping: false,
            registry: RegistryState::new(globals),
            seat: SeatState::new(globals, qh),
            outputs,
            compositor,
            shell,
            layer,
            settings,
            placement,
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
        self.layer.set_anchor(Anchor::TOP | Anchor::LEFT);
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
            .set_margin(self.placement.y, 0, 0, self.placement.x);
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
                        .placement
                        .monitor
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
        self.placement.x = (size.0 - self.width as i32) / 2;
        self.placement.y = (size.1 - self.height as i32) / 2;
        self.layer
            .set_margin(self.placement.y, 0, 0, self.placement.x);
        self.layer.commit();
        self.notify_position();
    }
    pub fn recreate(&mut self, qh: &QueueHandle<Self>) {
        let output = self.outputs.outputs().find(|o| {
            self.outputs.info(o).is_some_and(|i| {
                self.placement
                    .monitor
                    .as_ref()
                    .is_some_and(|n| i.name.as_ref() == Some(n))
            })
        });
        if let Some(ref o) = output {
            self.placement.monitor = self.outputs.info(o).and_then(|i| i.name);
        } else {
            self.placement.monitor = None;
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
            (f64::from(drag.margins.0) + delta.0).round() as i32,
            (f64::from(drag.margins.1) + delta.1).round() as i32,
        );
        let previous = (self.placement.x, self.placement.y);
        self.placement.x = desired.0;
        self.placement.y = desired.1;
        if previous != (self.placement.x, self.placement.y) {
            self.layer
                .set_margin(self.placement.y, 0, 0, self.placement.x);
            self.layer.commit();
            self.notify_position();
        }
    }
    pub fn notify_position(&self) {
        self.output
            .update_state(self.id, |state| state.placement = self.placement.clone());
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
