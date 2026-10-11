use super::{Connection, LayerSurface};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use smithay_client_toolkit::shell::WaylandSurface;
use std::ptr::NonNull;

/// Keeps the libwayland display and layer role alive for the WGPU surface.
#[derive(Debug, Clone)]
pub struct NativeWindow {
    pub(super) connection: Connection,
    pub(super) layer: LayerSurface,
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
