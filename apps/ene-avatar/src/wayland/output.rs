use super::*;

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
