use super::*;

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
                    self.drag = crate::render::mask::contains(&self.input_runs, event.position)
                        .then_some(Drag {
                            press: event.position,
                            latest: event.position,
                            margins: (self.placement.x, self.placement.y),
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
                    if self.drag.take().is_some() {
                        self.notify_position();
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
