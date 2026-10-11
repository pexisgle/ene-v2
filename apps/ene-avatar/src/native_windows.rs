//! GPU composition and input coverage are separate HWNDs: a region must not clip soft visual edges.
use crate::render::mask::Run;
use ene_ipc::renderer::{AvatarConfig, AvatarId, AvatarPlacement};
use raw_window_handle::*;
use std::{
    num::NonZeroIsize,
    sync::{Arc, Mutex},
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{DirectComposition::*, Gdi::*},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            HiDpi::*,
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, Interface, w},
};

#[derive(Debug)]
enum Event {
    Down(POINT),
    Move(POINT),
    Up(POINT),
    Cancel,
    Display,
    Dpi,
    Close,
}
#[derive(Debug)]
struct Context {
    events: Arc<Mutex<Vec<Event>>>,
}
#[derive(Debug)]
struct Windows {
    visual: HWND,
    input: HWND,
    _context: Box<Context>,
    device: IDCompositionDevice,
    _target: IDCompositionTarget,
    composition: IDCompositionVisual,
}
// HWNDs are borrowed by Bevy/WGPU; window operations remain on this process's main thread.
unsafe impl Send for Windows {}
unsafe impl Sync for Windows {}
impl Drop for Windows {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.input);
            let _ = DestroyWindow(self.visual);
        }
    }
}
#[derive(Debug, Clone)]
pub struct NativeWindow(Arc<Windows>);
impl HasWindowHandle for NativeWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let handle = Win32WindowHandle::new(
            NonZeroIsize::new(self.0.visual.0 as isize).ok_or(HandleError::Unavailable)?,
        );
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(handle)) })
    }
}
impl HasDisplayHandle for NativeWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(DisplayHandle::windows())
    }
}
impl NativeWindow {
    pub fn create_surface(
        &self,
        instance: &wgpu::Instance,
    ) -> Result<wgpu::Surface<'static>, wgpu::CreateSurfaceError> {
        // The Arc retains the visual/target/device until after the surface is dropped.
        unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CompositionVisual(
                self.0.composition.as_raw(),
            ))
        }
    }
    pub fn commit(&self) -> Result<(), windows::core::Error> {
        unsafe { self.0.device.Commit() }
    }
}
#[derive(Clone)]
struct Monitor {
    name: String,
    rect: RECT,
    primary: bool,
}
fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn collect(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let items = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(monitor, (&mut info as *mut MONITORINFOEXW).cast()) }.as_bool()
        {
            let len = info
                .szDevice
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.szDevice.len());
            items.push(Monitor {
                name: String::from_utf16_lossy(&info.szDevice[..len]),
                rect: info.monitorInfo.rcMonitor,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        BOOL(1)
    }
    let mut items = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM((&mut items as *mut Vec<Monitor>) as isize),
        );
    }
    items
}
unsafe extern "system" fn proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lp.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let context = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Context;
    if !context.is_null() {
        let context = unsafe { &*context };
        let mut point = POINT::default();
        match message {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_LBUTTONDOWN | WM_MOUSEMOVE | WM_LBUTTONUP => {
                unsafe {
                    let _ = GetCursorPos(&mut point);
                }
                let event = match message {
                    WM_LBUTTONDOWN => {
                        unsafe {
                            SetCapture(hwnd);
                        }
                        Event::Down(point)
                    }
                    WM_LBUTTONUP => {
                        // Queue release first: ReleaseCapture synchronously sends WM_CAPTURECHANGED.
                        context.events.lock().unwrap().push(Event::Up(point));
                        unsafe {
                            let _ = ReleaseCapture();
                        }
                        return LRESULT(0);
                    }
                    _ => Event::Move(point),
                };
                context.events.lock().unwrap().push(event);
                return LRESULT(0);
            }
            WM_CAPTURECHANGED => {
                context.events.lock().unwrap().push(Event::Cancel);
                return LRESULT(0);
            }
            WM_DISPLAYCHANGE => context.events.lock().unwrap().push(Event::Display),
            WM_DPICHANGED => context.events.lock().unwrap().push(Event::Dpi),
            WM_CLOSE => {
                context.events.lock().unwrap().push(Event::Close);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}
struct Drag {
    press: POINT,
    origin: POINT,
    moved: bool,
}
pub struct State {
    pub settings: AvatarConfig,
    pub placement: AvatarPlacement,
    pub id: AvatarId,
    pub output: crate::ipc::Output,
    pub requests: Vec<crate::ipc::WindowRequest>,
    pub stopping: bool,
    visible: bool,
    pub configured: bool,
    pub closed: bool,
    pub scale: u32,
    pub width: u32,
    pub height: u32,
    pub resized: bool,
    pub output_changed: bool,
    window: NativeWindow,
    events: Arc<Mutex<Vec<Event>>>,
    monitor: Monitor,
    dpi: f64,
    recenter: bool,
    drag: Option<Drag>,
    input_runs: Vec<Run>,
    mapped: bool,
}
impl State {
    pub fn new(
        id: AvatarId,
        settings: AvatarConfig,
        placement: AvatarPlacement,
        output: crate::ipc::Output,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(None)?;
            let class = w!("ene.avatar");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
                return Err(windows::core::Error::from_win32().into());
            }
            let all = monitors();
            let monitor = all
                .iter()
                .find(|m| placement.monitor.as_ref() == Some(&m.name))
                .or_else(|| all.iter().find(|m| m.primary))
                .or_else(|| all.first())
                .ok_or("no display is available")?
                .clone();
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut context = Box::new(Context {
                events: events.clone(),
            });
            let pointer = (&mut *context as *mut Context).cast();
            let common =
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP;
            let visual = CreateWindowExW(
                common | WS_EX_TRANSPARENT,
                class,
                w!("ene-avatar"),
                WS_POPUP,
                monitor.rect.left,
                monitor.rect.top,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                Some(pointer),
            )?;
            let input = match CreateWindowExW(
                common,
                class,
                w!("ene-avatar-input"),
                WS_POPUP,
                monitor.rect.left,
                monitor.rect.top,
                1,
                1,
                None,
                None,
                Some(instance.into()),
                Some(pointer),
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let _ = DestroyWindow(visual);
                    return Err(error.into());
                }
            };
            let composition_result = (|| -> windows::core::Result<_> {
                let device: IDCompositionDevice = DCompositionCreateDevice(None)?;
                let target = device.CreateTargetForHwnd(visual, true)?;
                let composition = device.CreateVisual()?;
                target.SetRoot(&composition)?;
                device.Commit()?;
                Ok((device, target, composition))
            })();
            let (device, target, composition) = match composition_result {
                Ok(result) => result,
                Err(error) => {
                    let _ = DestroyWindow(input);
                    let _ = DestroyWindow(visual);
                    return Err(error.into());
                }
            };
            let dpi = GetDpiForWindow(visual).max(96) as f64 / 96.;
            let window = NativeWindow(Arc::new(Windows {
                visual,
                input,
                _context: context,
                device,
                _target: target,
                composition,
            }));
            let mut state = Self {
                settings,
                placement,
                id,
                output,
                requests: Vec::new(),
                stopping: false,
                visible: true,
                configured: true,
                closed: false,
                scale: 1,
                width: 1,
                height: 1,
                resized: false,
                output_changed: false,
                window,
                events,
                monitor,
                dpi,
                recenter: false,
                drag: None,
                input_runs: Vec::new(),
                mapped: false,
            };
            state.properties();
            Ok(state)
        }
    }
    pub fn native_window(&self) -> NativeWindow {
        self.window.clone()
    }
    pub fn avatar_height(&self) -> f32 {
        (self.monitor.rect.bottom - self.monitor.rect.top) as f32
            * 0.4
            * self.settings.size_percent as f32
            / 100.
    }
    pub fn properties(&mut self) {
        let all = monitors();
        if let Some(monitor) = all
            .iter()
            .find(|m| self.placement.monitor.as_ref() == Some(&m.name))
            .or_else(|| all.iter().find(|m| m.primary))
            .or_else(|| all.first())
        {
            self.monitor = monitor.clone();
        }
        let h = (self.avatar_height() * 2.)
            .min((self.monitor.rect.bottom - self.monitor.rect.top) as f32)
            .round()
            .max(1.) as u32;
        let w = (self.avatar_height() * 1.8)
            .min((self.monitor.rect.right - self.monitor.rect.left) as f32)
            .round()
            .max(1.) as u32;
        if (self.width, self.height) != (w, h) {
            self.width = w;
            self.height = h;
            self.resized = true;
            self.clear_input();
        }
        let x = self.monitor.rect.left + (self.placement.x as f64 * self.dpi).round() as i32;
        let y = self.monitor.rect.top + (self.placement.y as f64 * self.dpi).round() as i32;
        unsafe {
            let _ = SetWindowPos(
                self.window.0.visual,
                Some(HWND_TOPMOST),
                x,
                y,
                self.width as i32,
                self.height as i32,
                SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                self.window.0.input,
                Some(self.window.0.visual),
                x,
                y,
                self.width as i32,
                self.height as i32,
                SWP_NOACTIVATE,
            );
            if !self.visible {
                let _ = ShowWindow(self.window.0.visual, SW_HIDE);
                let _ = ShowWindow(self.window.0.input, SW_HIDE);
                self.mapped = false;
                self.drag = None;
            }
        }
        self.placement.monitor = Some(self.monitor.name.clone());
    }
    fn clear_input(&mut self) {
        self.input_runs.clear();
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            if SetWindowRgn(self.window.0.input, Some(region), false) == 0 {
                let _ = DeleteObject(region.into());
            }
        }
    }
    pub fn prepare_gpu_present(&mut self, runs: &[Run]) {
        if self.mapped && self.input_runs == runs {
            return;
        }
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            if region.0.is_null() {
                return;
            }
            for (run, height) in crate::render::mask::rectangles(runs) {
                let part = CreateRectRgn(run.x, run.y, run.x + run.width, run.y + height);
                let combined = CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
                let _ = DeleteObject(part.into());
                if combined == RGN_ERROR {
                    let _ = DeleteObject(region.into());
                    self.clear_input();
                    return;
                }
            }
            if SetWindowRgn(self.window.0.input, Some(region), false) == 0 {
                let _ = DeleteObject(region.into());
                self.clear_input();
                return;
            }
            self.input_runs.clear();
            self.input_runs.extend_from_slice(runs);
            if !self.mapped && self.visible {
                let _ = ShowWindow(self.window.0.visual, SW_SHOWNOACTIVATE);
                let _ = ShowWindow(self.window.0.input, SW_SHOWNOACTIVATE);
                self.mapped = true;
            }
        }
    }
    pub fn request_recenter(&mut self) {
        self.recenter = true;
        self.drag = None;
    }
    pub fn apply_recenter(&mut self) {
        if !self.recenter {
            return;
        }
        self.recenter = false;
        self.placement.x = (((self.monitor.rect.right - self.monitor.rect.left) as f64
            - self.width as f64)
            / (2. * self.dpi))
            .round() as i32;
        self.placement.y = (((self.monitor.rect.bottom - self.monitor.rect.top) as f64
            - self.height as f64)
            / (2. * self.dpi))
            .round() as i32;
        self.properties();
        self.position();
    }
    fn position(&self) {
        self.output
            .update_state(self.id, |state| state.placement = self.placement.clone());
    }
    pub fn recreate(&mut self) {
        self.closed = false;
        self.clear_input();
        self.properties();
    }

    pub fn dispatch(&mut self, wait: Duration) {
        unsafe {
            let millis = wait.as_millis().min(u32::MAX as u128) as u32;
            let _ = MsgWaitForMultipleObjectsEx(None, millis, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        for event in events {
            match event {
                Event::Down(press) => {
                    let mut rect = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(self.window.0.visual, &mut rect);
                    }
                    self.drag = Some(Drag {
                        press,
                        origin: POINT {
                            x: rect.left,
                            y: rect.top,
                        },
                        moved: false,
                    })
                }
                Event::Move(point) => {
                    if let Some(drag) = &mut self.drag {
                        let dx = point.x - drag.press.x;
                        let dy = point.y - drag.press.y;
                        if dx.abs() + dy.abs() > (4. * self.dpi) as i32 {
                            drag.moved = true;
                        }
                        if drag.moved {
                            let origin = POINT {
                                x: drag.origin.x + dx,
                                y: drag.origin.y + dy,
                            };
                            self.place_at(origin);
                            self.properties();
                            self.position();
                        }
                    }
                }
                Event::Up(_point) => {
                    if let Some(drag) = self.drag.take() {
                        if drag.moved {
                            self.position();
                        } else {
                            self.position();
                        }
                    }
                }
                Event::Cancel => self.drag = None,
                Event::Display => {
                    self.output_changed = true;
                    self.request_recenter();
                }
                Event::Dpi => {
                    let mut rect = RECT::default();
                    unsafe {
                        let _ = GetWindowRect(self.window.0.visual, &mut rect);
                    }
                    self.dpi =
                        unsafe { GetDpiForWindow(self.window.0.visual) }.max(96) as f64 / 96.;
                    self.place_at(POINT {
                        x: rect.left,
                        y: rect.top,
                    });
                    self.resized = true;
                    self.clear_input();
                    self.properties();
                }
                Event::Close => self.closed = true,
            }
        }
    }
    fn place_at(&mut self, origin: POINT) {
        let all = monitors();
        let center = POINT {
            x: origin.x + self.width as i32 / 2,
            y: origin.y + self.height as i32 / 2,
        };
        if let Some(monitor) = all.iter().find(|m| {
            center.x >= m.rect.left
                && center.x < m.rect.right
                && center.y >= m.rect.top
                && center.y < m.rect.bottom
        }) {
            self.monitor = monitor.clone();
            self.placement.monitor = Some(monitor.name.clone());
        }
        self.placement.x = ((origin.x - self.monitor.rect.left) as f64 / self.dpi).round() as i32;
        self.placement.y = ((origin.y - self.monitor.rect.top) as f64 / self.dpi).round() as i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn state() -> Result<State, Box<dyn std::error::Error>> {
        State::for_test()
    }
    #[test]
    fn composition_and_input_windows_preserve_holes_without_activation() {
        let mut state = state().unwrap();
        let before = unsafe { GetForegroundWindow() };
        state.prepare_gpu_present(&[
            Run {
                x: 2,
                y: 3,
                width: 2,
            },
            Run {
                x: 6,
                y: 3,
                width: 2,
            },
            Run {
                x: 2,
                y: 4,
                width: 2,
            },
        ]);
        unsafe {
            let region = CreateRectRgn(0, 0, 0, 0);
            assert_ne!(GetWindowRgn(state.window.0.input, region), RGN_ERROR);
            assert!(PtInRegion(region, 2, 3).as_bool());
            assert!(PtInRegion(region, 3, 4).as_bool());
            assert!(!PtInRegion(region, 5, 3).as_bool());
            assert!(!PtInRegion(region, 7, 4).as_bool());
            let _ = DeleteObject(region.into());
            assert_eq!(GetForegroundWindow(), before);
            assert!(!IsWindowVisible(state.window.0.input).as_bool());
            assert!(!IsWindowVisible(state.window.0.visual).as_bool());
            assert_ne!(
                GetWindowLongPtrW(state.window.0.visual, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0,
                0
            );
        }
        // Recovery must allow constructing a fresh window in the same process.
        drop(state);
        drop(state().unwrap());
    }
}

#[cfg(test)]
impl State {
    pub(crate) fn for_test() -> Result<State, Box<dyn std::error::Error>> {
        let (states, _) = ene_ipc::remoc::rch::watch::channel(Default::default());
        let mut state = State::new(
            AvatarId::new(),
            AvatarConfig::default(),
            AvatarPlacement::default(),
            crate::ipc::Output {
                messages: None,
                states: Arc::new(states),
            },
        )?;
        state.visible = false;
        Ok(state)
    }
}
