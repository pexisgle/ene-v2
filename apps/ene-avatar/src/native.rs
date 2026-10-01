use crate::mask::Run;
use ene_shared::avatar::{AvatarEvent, AvatarSettings};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, QueueHandle,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

pub const WIDTH: u32 = 360;
pub const HEIGHT: u32 = 480;

pub struct State {
    pub registry: RegistryState,
    pub seat: SeatState,
    pub outputs: OutputState,
    pub compositor: CompositorState,
    shm: Shm,
    pool: SlotPool,
    pub shell: LayerShell,
    pub layer: LayerSurface,
    pub settings: AvatarSettings,
    pub configured: bool,
    pub closed: bool,
    pub scale: u32,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
    pub output_changed: bool,
    pointer: Option<wl_pointer::WlPointer>,
    drag: Option<((f64, f64), (i32, i32))>,
    input_runs: Vec<Run>,
}
impl State {
    pub fn new(
        globals: &wayland_client::globals::GlobalList,
        qh: &QueueHandle<Self>,
        settings: AvatarSettings,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let compositor = CompositorState::bind(globals, qh)?;
        let shell = LayerShell::bind(globals, qh).map_err(
            |_| "この環境はWayland layer-shellに対応していません（KDE/KWinなどの対応環境で起動してください）。",
        )?;
        let outputs = OutputState::new(globals, qh);
        let shm = Shm::bind(globals, qh)?;
        let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm)?;
        let surface = compositor.create_surface(qh);
        let layer = shell.create_layer_surface(qh, surface, Layer::Top, Some("ene-avatar"), None);
        let mut state = Self {
            registry: RegistryState::new(globals),
            seat: SeatState::new(globals, qh),
            outputs,
            shm,
            pool,
            compositor,
            shell,
            layer,
            settings,
            configured: false,
            closed: false,
            scale: 1,
            width: WIDTH,
            height: HEIGHT,
            resized: false,
            output_changed: false,
            pointer: None,
            drag: None,
            input_runs: Vec::new(),
        };
        state.properties();
        state.layer.commit();
        Ok(state)
    }
    pub fn properties(&mut self) {
        self.layer.set_anchor(Anchor::BOTTOM | Anchor::RIGHT);
        self.layer
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        self.layer.set_exclusive_zone(0);
        self.layer.set_size(WIDTH, HEIGHT);
        self.clamp_position();
        self.layer
            .set_margin(0, self.settings.right, self.settings.bottom, 0);
    }
    pub fn clamp_position(&mut self) {
        let size = self
            .outputs
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
            .unwrap_or((32768, 32768));
        self.settings.right = self
            .settings
            .right
            .clamp(0, (size.0 - self.width as i32).max(0));
        self.settings.bottom = self
            .settings
            .bottom
            .clamp(0, (size.1 - self.height as i32).max(0));
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
        self.configured = false;
        self.closed = false;
        self.scale = 1;
        self.properties();
        self.empty_input(qh);
        self.layer.commit();
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
        for run in runs {
            region.add(run.x, run.y, run.width, 1);
        }
        self.layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));
    }
    pub fn present(&mut self, rgba: &[u8], runs: &[Run]) -> Result<(), Box<dyn std::error::Error>> {
        let width = self.width * self.scale;
        let height = self.height * self.scale;
        if rgba.len() != (width * height * 4) as usize {
            return Err("描画フレームの寸法が一致しません。".into());
        }
        let (buffer, canvas) = self.pool.create_buffer(
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
        )?;
        // Convert the SRGB render target to Wayland premultiplied ARGB8888
        // (BGRA bytes on the little-endian target), preserving source alpha.
        for (src, dst) in rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(canvas.as_chunks_mut::<4>().0.iter_mut())
        {
            *dst = crate::pixels::argb(src);
        }
        self.set_input(runs);
        self.input_runs = runs.to_vec();
        self.layer.wl_surface().set_buffer_scale(self.scale as i32);
        buffer.attach_to(self.layer.wl_surface())?;
        self.layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        // Pixels and the exact alpha-derived input region enter the compositor
        // in ONE commit. No invisible old arm can intercept a click.
        self.layer.commit();
        Ok(())
    }
    pub fn empty_input(&mut self, _qh: &QueueHandle<Self>) {
        self.input_runs.clear();
        self.set_input(&[]);
        self.layer.commit();
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
            self.settings.output = self.outputs.info(output).and_then(|i| i.name);
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
        self.clamp_position();
        self.notify_position();
        if self.settings.visible {
            self.properties();
            self.layer.commit();
        }
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.output_changed = true;
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
        self.width = if c.new_size.0 == 0 {
            WIDTH
        } else {
            c.new_size.0
        };
        self.height = if c.new_size.1 == 0 {
            HEIGHT
        } else {
            c.new_size.1
        };
        self.configured = true;
        self.resized = true;
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
        if std::env::var_os("ENE_AVATAR_DEBUG").is_some() {
            eprintln!("seat capability {c:?}");
        }
        if c == Capability::Pointer && self.pointer.is_none() {
            match self.seat.get_pointer(qh, &seat) {
                Ok(pointer) => self.pointer = Some(pointer),
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
            p.release();
            self.drag = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {
        self.drag = None;
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
            if std::env::var_os("ENE_AVATAR_DEBUG").is_some() {
                eprintln!(
                    "pointer {:?} at {:?}, avatar={}",
                    event.kind,
                    event.position,
                    &event.surface == self.layer.wl_surface()
                );
            }
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Press { button: 0x110, .. } => {
                    // Also reject a stale compositor focus after an arm moves
                    // away underneath a stationary mouse cursor.
                    self.drag = crate::mask::contains(&self.input_runs, event.position)
                        .then_some((event.position, (self.settings.right, self.settings.bottom)));
                }
                PointerEventKind::Motion { .. } => {
                    if let Some((start, _)) = self.drag {
                        self.settings.right -= (event.position.0 - start.0).round() as i32;
                        self.settings.bottom -= (event.position.1 - start.1).round() as i32;
                        self.clamp_position();
                        self.layer
                            .set_margin(0, self.settings.right, self.settings.bottom, 0);
                        self.layer.commit();
                    }
                }
                PointerEventKind::Release { button: 0x110, .. } => {
                    if let Some((_, start)) = self.drag.take() {
                        if (self.settings.right - start.0).abs()
                            + (self.settings.bottom - start.1).abs()
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
impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}
smithay_client_toolkit::delegate_registry!(State);
smithay_client_toolkit::delegate_dispatch2!(State);

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
