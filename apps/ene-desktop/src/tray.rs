//! Taskbar notification area (system tray) residency for `ene-desktop`.
//!
//! Runs a dedicated message-loop thread on Windows so modal Win32 context menus
//! never block GPUI's UI thread, and reports user selections back over an
//! unbounded channel. On unsupported platforms `Tray::spawn` returns `None` so
//! the client degrades cleanly to standard window-lifetime shutdown.

use crate::ui::{PodMode, SettingsSection};
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::UnboundedSender;

/// Commands emitted from the taskbar notification area icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ActivatePod,
    OpenChat,
    OpenMiniPod,
    HidePod,
    OpenSettings(SettingsSection),
    CenterAvatar,
    ToggleMicMute,
    Quit,
}

/// Live client summary reflected in the tray tooltip and context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayState {
    pub pod_open: bool,
    pub pod_mode: PodMode,
    pub mic_muted: bool,
    pub avatar_ready: bool,
    pub tooltip: String,
}

impl Default for TrayState {
    fn default() -> Self {
        Self {
            pod_open: true,
            pod_mode: PodMode::MiniVoice,
            mic_muted: false,
            avatar_ready: false,
            tooltip: "ene".to_owned(),
        }
    }
}

/// Cloneable UI-side handle to refresh tray state and check residency.
#[derive(Clone)]
pub struct TrayHandle {
    shared: Arc<RwLock<TrayState>>,
    #[cfg(windows)]
    hwnd_raw: isize,
    resident: Arc<AtomicBool>,
}

impl TrayHandle {
    pub fn is_resident(&self) -> bool {
        self.resident.load(Ordering::Acquire)
    }

    pub fn update(&self, next: TrayState) {
        let tip_changed = match self.shared.write() {
            Ok(mut guard) => {
                let changed = guard.tooltip != next.tooltip;
                *guard = next;
                changed
            }
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                let changed = guard.tooltip != next.tooltip;
                *guard = next;
                changed
            }
        };
        #[cfg(windows)]
        if self.is_resident() && tip_changed {
            windows_tray::request_tip_refresh(self.hwnd_raw);
        }
        #[cfg(not(windows))]
        let _ = tip_changed;
    }
}

/// Owned tray lifecycle guard; closes the tray thread and removes the icon on exit.
pub struct Tray {
    handle: TrayHandle,
    #[cfg(windows)]
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Tray {
    pub fn spawn(initial: TrayState, tx: UnboundedSender<TrayAction>) -> Option<Self> {
        #[cfg(windows)]
        {
            windows_tray::spawn(initial, tx)
        }
        #[cfg(not(windows))]
        {
            let _ = (initial, tx);
            None
        }
    }

    pub fn handle(&self) -> TrayHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        #[cfg(windows)]
        {
            windows_tray::request_close(self.handle.hwnd_raw);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(any(windows, test))]
const CMD_OPEN_CHAT: u32 = 1;
#[cfg(any(windows, test))]
const CMD_OPEN_MINI: u32 = 2;
#[cfg(any(windows, test))]
const CMD_HIDE_POD: u32 = 3;
#[cfg(any(windows, test))]
const CMD_SETTINGS_COMPANIONS: u32 = 4;
#[cfg(any(windows, test))]
const CMD_SETTINGS_CONNECTIONS: u32 = 5;
#[cfg(any(windows, test))]
const CMD_SETTINGS_DEVICE: u32 = 6;
#[cfg(any(windows, test))]
const CMD_CENTER_AVATAR: u32 = 7;
#[cfg(any(windows, test))]
const CMD_TOGGLE_MIC: u32 = 8;
#[cfg(any(windows, test))]
const CMD_QUIT: u32 = 9;

#[cfg(any(windows, test))]
fn action_for_command(id: u32) -> Option<TrayAction> {
    match id {
        CMD_OPEN_CHAT => Some(TrayAction::OpenChat),
        CMD_OPEN_MINI => Some(TrayAction::OpenMiniPod),
        CMD_HIDE_POD => Some(TrayAction::HidePod),
        CMD_SETTINGS_COMPANIONS => Some(TrayAction::OpenSettings(SettingsSection::Companions)),
        CMD_SETTINGS_CONNECTIONS => Some(TrayAction::OpenSettings(SettingsSection::Connections)),
        CMD_SETTINGS_DEVICE => Some(TrayAction::OpenSettings(SettingsSection::Device)),
        CMD_CENTER_AVATAR => Some(TrayAction::CenterAvatar),
        CMD_TOGGLE_MIC => Some(TrayAction::ToggleMicMute),
        CMD_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

/// Encodes `text` as a NUL-terminated UTF-16 buffer fitting `NOTIFYICONDATAW::szTip`
/// (`[u16; 128]`), stopping cleanly before splitting a surrogate pair.
#[cfg(any(windows, test))]
fn encode_tooltip(text: &str) -> [u16; 128] {
    let mut buf = [0u16; 128];
    let limit = buf.len() - 1;
    let mut len = 0usize;
    for ch in text.chars() {
        let mut units = [0u16; 2];
        let encoded = ch.encode_utf16(&mut units);
        if len + encoded.len() > limit {
            break;
        }
        buf[len..len + encoded.len()].copy_from_slice(encoded);
        len += encoded.len();
    }
    buf
}

#[cfg(windows)]
mod windows_tray {
    use super::*;
    use std::{cell::Cell, ffi::c_void};
    use windows::{
        Win32::{
            Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
            Graphics::Gdi::{
                BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection,
                DIB_RGB_COLORS, DeleteObject,
            },
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                Shell::{
                    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                    NOTIFYICONDATAW, Shell_NotifyIconW,
                },
                WindowsAndMessaging::{
                    AppendMenuW, CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreateIconIndirect,
                    CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
                    DestroyWindow, DispatchMessageW, GWL_USERDATA, GetCursorPos, GetMessageW,
                    GetSystemMetrics, GetWindowLongPtrW, HICON, HMENU, ICONINFO, IMAGE_FLAGS,
                    IMAGE_ICON, KillTimer, LoadIconW, LoadImageW, MENU_ITEM_FLAGS, MF_CHECKED,
                    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, MSGFLT_ALLOW, PostMessageW,
                    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
                    SM_CYSMICON, SetForegroundWindow, SetMenuDefaultItem, SetTimer,
                    SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
                    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, USER_DEFAULT_SCREEN_DPI,
                    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
                    WM_NCCREATE, WM_NCDESTROY, WM_NULL, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
                    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
                },
            },
        },
        core::{PCWSTR, w},
    };

    const TRAY_ICON_ID: u32 = 1;
    const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
    const WM_TRAY_UPDATE: u32 = WM_APP + 2;
    const TRAY_RETRY_TIMER_ID: usize = 1;
    const TRAY_RETRY_INTERVAL_MS: u32 = 2_000;
    const TRAY_RETRY_MAX_TICKS: u32 = 15;

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(pv_reserved: *const c_void, dw_co_init: u32) -> i32;
        fn CoUninitialize();
    }

    struct WindowContext {
        shared: Arc<RwLock<TrayState>>,
        tx: UnboundedSender<TrayAction>,
        icon: HICON,
        icon_owned: bool,
        taskbar_created_msg: u32,
        resident: Arc<AtomicBool>,
        added: Cell<bool>,
        retry_ticks: Cell<u32>,
    }

    pub(super) fn spawn(initial: TrayState, tx: UnboundedSender<TrayAction>) -> Option<Tray> {
        let shared = Arc::new(RwLock::new(initial));
        let resident = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_resident = Arc::clone(&resident);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<isize, ()>>(1);

        let thread = std::thread::Builder::new()
            .name("ene-tray".into())
            .spawn(move || {
                run_thread(thread_shared, thread_resident, tx, ready_tx);
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Ok(hwnd_raw)) => Some(Tray {
                handle: TrayHandle {
                    shared,
                    hwnd_raw,
                    resident,
                },
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                None
            }
        }
    }

    pub(super) fn request_tip_refresh(hwnd_raw: isize) {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_TRAY_UPDATE, WPARAM(0), LPARAM(0));
        }
    }

    pub(super) fn request_close(hwnd_raw: isize) {
        let hwnd = HWND(hwnd_raw as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }

    fn run_thread(
        shared: Arc<RwLock<TrayState>>,
        resident: Arc<AtomicBool>,
        tx: UnboundedSender<TrayAction>,
        ready_tx: std::sync::mpsc::SyncSender<Result<isize, ()>>,
    ) {
        const COINIT_APARTMENTTHREADED: u32 = 0x2;
        let com_init = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED) };

        let class_name = w!("EneDesktopTrayHost");
        let instance = unsafe { GetModuleHandleW(None) }.ok();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.unwrap_or_default().into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = unsafe { RegisterClassW(&wc) };

        let (icon, icon_owned) = load_or_create_tray_icon();
        let taskbar_created_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        let ctx = Box::new(WindowContext {
            shared,
            tx,
            icon,
            icon_owned,
            taskbar_created_msg,
            resident,
            added: Cell::new(false),
            retry_ticks: Cell::new(0),
        });
        let ctx_ptr = Box::into_raw(ctx);

        // Hidden top-level overlapped window (never shown, not HWND_MESSAGE) so
        // broadcast `TaskbarCreated` messages reach it if explorer.exe starts late
        // or restarts.
        let hwnd = match unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class_name,
                w!("ene-tray"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                0,
                CW_USEDEFAULT,
                0,
                None,
                None,
                instance.map(Into::into),
                Some(ctx_ptr.cast()),
            )
        } {
            Ok(hwnd) => hwnd,
            Err(error) => {
                tracing::warn!("system tray host window failed: {error}");
                let owned = unsafe { Box::from_raw(ctx_ptr) };
                if owned.icon_owned && !owned.icon.is_invalid() {
                    unsafe {
                        let _ = DestroyIcon(owned.icon);
                    }
                }
                if com_init >= 0 {
                    unsafe { CoUninitialize() };
                }
                let _ = ready_tx.send(Err(()));
                return;
            }
        };

        unsafe {
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_TRAY_CALLBACK, MSGFLT_ALLOW, None);
            if taskbar_created_msg != 0 {
                let _ = ChangeWindowMessageFilterEx(hwnd, taskbar_created_msg, MSGFLT_ALLOW, None);
            }
        }

        let ctx_ref = unsafe { &*ctx_ptr };
        try_register_tray_icon(hwnd, ctx_ref);
        let _ = ready_tx.send(Ok(hwnd.0 as isize));

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if com_init >= 0 {
            unsafe { CoUninitialize() };
        }
    }

    fn current_state(ctx: &WindowContext) -> TrayState {
        match ctx.shared.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn build_nid(hwnd: HWND, ctx: &WindowContext, tooltip: &str) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage: WM_TRAY_CALLBACK,
            hIcon: ctx.icon,
            szTip: encode_tooltip(tooltip),
            ..Default::default()
        }
    }

    fn try_register_tray_icon(hwnd: HWND, ctx: &WindowContext) {
        let state = current_state(ctx);
        let nid = build_nid(hwnd, ctx, &state.tooltip);
        let mut last_err = None;
        for attempt in 0..5 {
            let msg = if ctx.added.get() { NIM_MODIFY } else { NIM_ADD };
            if unsafe { Shell_NotifyIconW(msg, &nid) }.as_bool() {
                ctx.added.set(true);
                ctx.retry_ticks.set(0);
                ctx.resident.store(true, Ordering::Release);
                unsafe {
                    let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                }
                tracing::info!("system tray icon registered");
                return;
            }
            last_err = Some(windows::core::Error::from_thread());
            if attempt + 1 < 5 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        if let Some(err) = last_err {
            tracing::warn!(error = %err, "Shell_NotifyIconW(NIM_ADD) failed; scheduling retry");
        } else {
            tracing::warn!("Shell_NotifyIconW(NIM_ADD) failed; scheduling retry");
        }
        unsafe {
            let _ = SetTimer(
                Some(hwnd),
                TRAY_RETRY_TIMER_ID,
                TRAY_RETRY_INTERVAL_MS,
                None,
            );
        }
    }

    fn refresh_tray_tip(hwnd: HWND, ctx: &WindowContext) {
        let state = current_state(ctx);
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_TIP,
            szTip: encode_tooltip(&state.tooltip),
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn delete_tray_icon(hwnd: HWND) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_NCCREATE {
            let cs = lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            if !cs.is_null() {
                let ctx_ptr = unsafe { (*cs).lpCreateParams } as isize;
                unsafe {
                    SetWindowLongPtrW(hwnd, GWL_USERDATA, ctx_ptr);
                }
            }
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }

        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWL_USERDATA) } as *mut WindowContext;
        if ptr.is_null() {
            return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
        }
        let ctx = unsafe { &*ptr };

        if ctx.taskbar_created_msg != 0 && msg == ctx.taskbar_created_msg {
            ctx.added.set(false);
            ctx.retry_ticks.set(0);
            try_register_tray_icon(hwnd, ctx);
            return LRESULT(0);
        }

        match msg {
            WM_TIMER if wparam.0 == TRAY_RETRY_TIMER_ID => {
                if ctx.added.get() {
                    unsafe {
                        let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                    }
                } else {
                    let ticks = ctx.retry_ticks.get().saturating_add(1);
                    ctx.retry_ticks.set(ticks);
                    try_register_tray_icon(hwnd, ctx);
                    if !ctx.added.get() && ticks >= TRAY_RETRY_MAX_TICKS {
                        unsafe {
                            let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_TRAY_UPDATE => {
                if ctx.added.get() {
                    refresh_tray_tip(hwnd, ctx);
                }
                LRESULT(0)
            }
            WM_TRAY_CALLBACK => {
                let event = (lparam.0 as u32) & 0xFFFF;
                match event {
                    WM_LBUTTONUP => {
                        let _ = ctx.tx.send(TrayAction::ActivatePod);
                    }
                    WM_LBUTTONDBLCLK => {
                        let _ = ctx.tx.send(TrayAction::OpenChat);
                    }
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        show_context_menu(hwnd, ctx);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                }
                if ctx.added.get() {
                    delete_tray_icon(hwnd);
                    ctx.added.set(false);
                }
                ctx.resident.store(false, Ordering::Release);
                unsafe { PostQuitMessage(0) };
                LRESULT(0)
            }
            WM_NCDESTROY => {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWL_USERDATA, 0);
                }
                let owned = unsafe { Box::from_raw(ptr) };
                if owned.icon_owned && !owned.icon.is_invalid() {
                    unsafe {
                        let _ = DestroyIcon(owned.icon);
                    }
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn show_context_menu(hwnd: HWND, ctx: &WindowContext) {
        let state = current_state(ctx);
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };

        let chat_flags = if state.pod_open && state.pod_mode == PodMode::ExpandedChat {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let mini_flags = if state.pod_open && state.pod_mode == PodMode::MiniVoice {
            MF_STRING | MF_CHECKED
        } else {
            MF_STRING
        };
        let hide_flags = if state.pod_open {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        let avatar_flags = if state.avatar_ready {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        let mic_label = if state.mic_muted {
            w!("マイクのミュートを解除")
        } else {
            w!("マイクをミュート")
        };

        append_item(menu, chat_flags, CMD_OPEN_CHAT, w!("チャット画面を開く"));
        append_item(menu, mini_flags, CMD_OPEN_MINI, w!("ミニ対話ポッドを表示"));
        append_item(menu, hide_flags, CMD_HIDE_POD, w!("対話ポッドを隠す"));
        append_separator(menu);
        append_item(
            menu,
            MF_STRING,
            CMD_SETTINGS_COMPANIONS,
            w!("設定画面を開く"),
        );
        append_item(
            menu,
            MF_STRING,
            CMD_SETTINGS_CONNECTIONS,
            w!("AI・音声の接続設定"),
        );
        append_item(menu, MF_STRING, CMD_SETTINGS_DEVICE, w!("この端末の設定"));
        append_separator(menu);
        append_item(
            menu,
            avatar_flags,
            CMD_CENTER_AVATAR,
            w!("アバターを中央に戻す"),
        );
        append_item(menu, MF_STRING, CMD_TOGGLE_MIC, mic_label);
        append_separator(menu);
        append_item(menu, MF_STRING, CMD_QUIT, w!("終了"));

        if !state.pod_open {
            unsafe {
                let _ = SetMenuDefaultItem(menu, CMD_OPEN_CHAT, 0);
            }
        }

        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
        }
        let picked = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
                pt.x,
                pt.y,
                None,
                hwnd,
                None,
            )
        };
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
        if picked.0 > 0
            && let Some(action) = action_for_command(picked.0 as u32)
        {
            let _ = ctx.tx.send(action);
        }
    }

    fn append_item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, label: PCWSTR) {
        unsafe {
            let _ = AppendMenuW(menu, flags, id as usize, label);
        }
    }

    fn append_separator(menu: HMENU) {
        unsafe {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
    }

    fn load_or_create_tray_icon() -> (HICON, bool) {
        let cx = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
        let cy = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
        if let Ok(module) = unsafe { GetModuleHandleW(None) } {
            if let Ok(handle) = unsafe {
                LoadImageW(
                    Some(module.into()),
                    PCWSTR(std::ptr::without_provenance(1)),
                    IMAGE_ICON,
                    cx,
                    cy,
                    IMAGE_FLAGS(0),
                )
            } && !handle.is_invalid()
            {
                return (HICON(handle.0), true);
            }
            if let Ok(icon) =
                unsafe { LoadIconW(Some(module.into()), PCWSTR(std::ptr::without_provenance(1))) }
                && !icon.is_invalid()
            {
                return (icon, false);
            }
        }
        (
            create_fallback_icon(cx.min(64) as u32).unwrap_or_default(),
            true,
        )
    }

    fn create_fallback_icon(size: u32) -> Option<HICON> {
        let size = size.max(16);
        let stride = (size * 4) as usize;
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color =
            unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
        if bits.is_null() {
            unsafe {
                let _ = DeleteObject(color.into());
            }
            return None;
        }
        let pixels =
            unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), stride * (size as usize)) };
        let s = size as f32;
        let c = (s - 1.0) * 0.5;
        let r = s * 0.45;
        let _ = USER_DEFAULT_SCREEN_DPI;
        for y in 0..size {
            for x in 0..size {
                let dx = (x as f32 - c) / r;
                let dy = (y as f32 - c) / r;
                let dist = (dx * dx + dy * dy).sqrt();
                let off = ((y * size + x) * 4) as usize;
                if dist <= 1.0 {
                    let star = dx.abs().sqrt() + dy.abs().sqrt() <= 0.85;
                    let (pr, pg, pb) = if star {
                        (255, 253, 253)
                    } else {
                        (207, 46, 104)
                    };
                    pixels[off] = pb;
                    pixels[off + 1] = pg;
                    pixels[off + 2] = pr;
                    pixels[off + 3] = 255;
                } else {
                    pixels[off..off + 4].fill(0);
                }
            }
        }
        let mask_stride = (size.div_ceil(16) * 2) as usize;
        let mask_bytes = vec![0u8; mask_stride * (size as usize)];
        let mask = unsafe {
            CreateBitmap(
                size as i32,
                size as i32,
                1,
                1,
                Some(mask_bytes.as_ptr().cast()),
            )
        };
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = unsafe { CreateIconIndirect(&info) }.ok();
        unsafe {
            if !mask.is_invalid() {
                let _ = DeleteObject(mask.into());
            }
            let _ = DeleteObject(color.into());
        }
        icon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_encoding_truncates_safely_on_utf16_and_surrogate_boundaries() {
        let ascii = encode_tooltip("ene - 待機中");
        let end = ascii.iter().position(|&c| c == 0).unwrap();
        assert_eq!(String::from_utf16(&ascii[..end]).unwrap(), "ene - 待機中");

        // Fill 126 code units with 'a' followed by a 2-unit emoji; the emoji must not
        // be split across index 127 because index 127 must remain the NUL terminator.
        let mut s = "a".repeat(126);
        s.push('🌸');
        let encoded = encode_tooltip(&s);
        assert_eq!(encoded[127], 0);
        let end = encoded.iter().position(|&c| c == 0).unwrap();
        assert_eq!(end, 126);
        assert!(String::from_utf16(&encoded[..end]).is_ok());
    }

    #[test]
    fn menu_command_ids_map_to_expected_actions() {
        assert_eq!(
            action_for_command(CMD_OPEN_CHAT),
            Some(TrayAction::OpenChat)
        );
        assert_eq!(
            action_for_command(CMD_OPEN_MINI),
            Some(TrayAction::OpenMiniPod)
        );
        assert_eq!(action_for_command(CMD_HIDE_POD), Some(TrayAction::HidePod));
        assert_eq!(
            action_for_command(CMD_SETTINGS_COMPANIONS),
            Some(TrayAction::OpenSettings(SettingsSection::Companions))
        );
        assert_eq!(
            action_for_command(CMD_SETTINGS_CONNECTIONS),
            Some(TrayAction::OpenSettings(SettingsSection::Connections))
        );
        assert_eq!(
            action_for_command(CMD_SETTINGS_DEVICE),
            Some(TrayAction::OpenSettings(SettingsSection::Device))
        );
        assert_eq!(
            action_for_command(CMD_CENTER_AVATAR),
            Some(TrayAction::CenterAvatar)
        );
        assert_eq!(
            action_for_command(CMD_TOGGLE_MIC),
            Some(TrayAction::ToggleMicMute)
        );
        assert_eq!(action_for_command(CMD_QUIT), Some(TrayAction::Quit));
        assert_eq!(action_for_command(0), None);
        assert_eq!(action_for_command(999), None);
    }
}
