use super::*;
use super::{icon::load_or_create_tray_icon, menu};
use std::{cell::Cell, ffi::c_void};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                CW_USEDEFAULT, ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW,
                DestroyIcon, DestroyWindow, DispatchMessageW, GWL_USERDATA, GetMessageW,
                GetWindowLongPtrW, HICON, KillTimer, MSG, MSGFLT_ALLOW, PostMessageW,
                PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetTimer,
                SetWindowLongPtrW, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY,
                WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCCREATE, WM_NCDESTROY, WM_RBUTTONUP, WM_TIMER,
                WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
            },
        },
    },
    core::w,
};

const TRAY_ICON_ID: u32 = 1;
const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const WM_TRAY_UPDATE: u32 = WM_APP + 2;
const TRAY_RETRY_TIMER_ID: usize = 1;
const TRAY_RETRY_INTERVAL_MS: u32 = 2_000;
const TRAY_RETRY_MAX_TICKS: u32 = 15;

#[link(name = "ole32")]
// SAFETY: these are the flat-API entry points `combaseapi.h`
// declares; they take no borrowed state and are safe to name. The
// calls that use them carry their own justifications below.
unsafe extern "system" {
    fn CoInitializeEx(pv_reserved: *const c_void, dw_co_init: u32) -> i32;
    fn CoUninitialize();
}

pub(super) struct WindowContext {
    shared: Arc<RwLock<TrayState>>,
    pub(super) tx: UnboundedSender<TrayAction>,
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
            let _discarded = thread.join();
            None
        }
    }
}

pub(super) fn request_tip_refresh(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    // SAFETY: `hwnd_raw` names the tray window this module created; a
    // `PostMessageW` to a window that has since closed returns an error
    // rather than dereferencing anything, so a stale id is not unsound.
    unsafe {
        drop(PostMessageW(
            Some(hwnd),
            WM_TRAY_UPDATE,
            WPARAM(0),
            LPARAM(0),
        ));
    }
}

pub(super) fn request_close(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut c_void);
    // SAFETY: as in `request_tip_refresh`; posting to a closed window is
    // reported to the caller rather than being unsound.
    unsafe {
        ignore_teardown(PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)));
    }
}

fn run_thread(
    shared: Arc<RwLock<TrayState>>,
    resident: Arc<AtomicBool>,
    tx: UnboundedSender<TrayAction>,
    ready_tx: std::sync::mpsc::SyncSender<Result<isize, ()>>,
) {
    const COINIT_APARTMENTTHREADED: u32 = 0x2;
    // SAFETY: a null reserved pointer is what `CoInitializeEx`
    // requires; this thread pairs the call with `CoUninitialize`
    // before it exits, on every path.
    let com_init = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED) };

    let class_name = w!("EneDesktopTrayHost");
    // SAFETY: `None` asks for this process's own module, which the
    // loader keeps alive for the whole process lifetime.
    let instance = unsafe { GetModuleHandleW(None) }.ok();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.unwrap_or_default().into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // SAFETY: `wc` is a fully initialised `WNDCLASSW` that outlives the
    // call; `wnd_proc` is a valid callback with the required signature.
    let _discarded = unsafe { RegisterClassW(&wc) };

    let (icon, icon_owned) = load_or_create_tray_icon();
    // SAFETY: the literal is a NUL-terminated UTF-16 string with static
    // lifetime, which is all `RegisterWindowMessageW` reads.
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
    // SAFETY: the class was registered above on this thread and
    // `ctx_ptr` is the live box pointer handed over for `WM_NCCREATE`
    // to store; ownership of it transfers to the window procedure.
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
            // SAFETY: `CreateWindowExW` failed, so no window took
            // ownership at `WM_NCCREATE`; this is the only other place
            // that can reclaim the box, and it runs once.
            let owned = unsafe { Box::from_raw(ctx_ptr) };
            if owned.icon_owned && !owned.icon.is_invalid() {
                // SAFETY: the icon was created by this module and
                // `icon_owned` records that this module must destroy it.
                unsafe {
                    let _discarded = DestroyIcon(owned.icon);
                }
            }
            if com_init >= 0 {
                // SAFETY: paired with the `CoInitializeEx` above, which
                // returned success; unbalanced otherwise and skipped.
                unsafe { CoUninitialize() };
            }
            let _discarded = ready_tx.send(Err(()));
            return;
        }
    };

    // SAFETY: `hwnd` is the window created immediately above on this
    // thread, and the call only lowers the filter for the given message.
    unsafe {
        let _discarded = ChangeWindowMessageFilterEx(hwnd, WM_TRAY_CALLBACK, MSGFLT_ALLOW, None);
    }
    if taskbar_created_msg != 0 {
        // SAFETY: same live window; the message id came from the
        // registration above and is only compared, never dereferenced.
        unsafe {
            let _discarded =
                ChangeWindowMessageFilterEx(hwnd, taskbar_created_msg, MSGFLT_ALLOW, None);
        }
    }

    // SAFETY: the window was created, so `WM_NCCREATE` stored `ctx_ptr`
    // and it stays owned by that window until `WM_NCDESTROY`. This
    // borrow ends before the message loop starts.
    let ctx_ref = unsafe { &*ctx_ptr };
    try_register_tray_icon(hwnd, ctx_ref);
    let _discarded = ready_tx.send(Ok(hwnd.0 as isize));

    let mut msg = MSG::default();
    // SAFETY: `msg` is a live local this thread exclusively owns, and
    // the loop runs on the thread that created the window.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        // SAFETY: `msg` was filled by `GetMessageW` on this thread.
        unsafe {
            let _discarded = TranslateMessage(&msg);
        }
        // SAFETY: same message, dispatched on the thread that owns the
        // window, which is what the Win32 message loop requires.
        unsafe {
            DispatchMessageW(&msg);
        }
    }
    if com_init >= 0 {
        // SAFETY: the loop has exited, so this thread no longer uses COM;
        // the call is balanced against the successful init above.
        unsafe { CoUninitialize() };
    }
}

pub(super) fn current_state(ctx: &WindowContext) -> TrayState {
    ctx.shared.read().clone()
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

fn try_send_tray_icon(
    hwnd: HWND,
    ctx: &WindowContext,
    nid: &NOTIFYICONDATAW,
) -> Option<windows::core::Error> {
    let msg = if ctx.added.get() { NIM_MODIFY } else { NIM_ADD };
    // SAFETY: `nid` is a live local whose `hWnd` the caller derived
    // from this thread's window; the shell copies what it needs.
    if unsafe { Shell_NotifyIconW(msg, nid) }.as_bool() {
        ctx.added.set(true);
        ctx.retry_ticks.set(0);
        ctx.resident.store(true, Ordering::Release);
        // SAFETY: `hwnd` is the tray window; killing a timer that was
        // set on it is a no-op when it has none.
        unsafe {
            let _discarded = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
        }
        tracing::info!("system tray icon registered");
        return None;
    }
    Some(windows::core::Error::from_thread())
}

fn schedule_tray_retry(hwnd: HWND, last_err: Option<windows::core::Error>) {
    if let Some(err) = last_err {
        tracing::warn!(error = %err, "Shell_NotifyIconW(NIM_ADD) failed; scheduling retry");
    } else {
        tracing::warn!("Shell_NotifyIconW(NIM_ADD) failed; scheduling retry");
    }
    // SAFETY: `hwnd` is the tray window and the callback is `None`, so
    // the timer posts `WM_TIMER` to that window rather than calling back.
    unsafe {
        let _discarded = SetTimer(
            Some(hwnd),
            TRAY_RETRY_TIMER_ID,
            TRAY_RETRY_INTERVAL_MS,
            None,
        );
    }
}

fn try_register_tray_icon(hwnd: HWND, ctx: &WindowContext) {
    let state = current_state(ctx);
    let nid = build_nid(hwnd, ctx, &state.tooltip);
    let mut last_err = None;
    for attempt in 0..5 {
        match try_send_tray_icon(hwnd, ctx, &nid) {
            None => return,
            Some(err) => last_err = Some(err),
        }
        if attempt + 1 < 5 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    schedule_tray_retry(hwnd, last_err);
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
    // SAFETY: `nid` is a live local naming a tooltip buffer this call
    // fills; the shell copies the tooltip before returning.
    unsafe {
        ignore_teardown(Shell_NotifyIconW(NIM_MODIFY, &nid));
    }
}

fn delete_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    // SAFETY: removal names the icon by window and id only; it reads no
    // borrowed state, so it stays valid even as the window is destroyed.
    unsafe {
        ignore_teardown(Shell_NotifyIconW(NIM_DELETE, &nid));
    }
}

// SAFETY: this is a callback the OS invokes with the arguments it
// documents for a `WNDPROC`; every dereference below is justified at
// its own site.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
        if !cs.is_null() {
            // SAFETY: `WM_NCCREATE` delivers a live `CREATESTRUCTW` as
            // `lparam`; the null check above guarantees it is readable.
            let ctx_ptr = unsafe { (*cs).lpCreateParams } as isize;
            // SAFETY: the window is live and being created; the user-data
            // slot is free until this call and holds no pointer before it.
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_USERDATA, ctx_ptr);
            }
        }
        // SAFETY: default handling for a message this procedure does not
        // own; the arguments were supplied by the OS.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // SAFETY: reading a stored user-data slot on a live window; the value
    // is our own box pointer or zero, and zero is rejected below.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWL_USERDATA) } as *mut WindowContext;
    if ptr.is_null() {
        // SAFETY: same as the default branch above; nothing was stored.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `ptr` is the box handed over at `WM_NCCREATE` and it is only
    // freed in `WM_NCDESTROY`, so it is live for every message between.
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
                // SAFETY: this window's retry timer is no longer needed
                // because the icon is registered.
                unsafe {
                    let _discarded = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
                }
            } else {
                let ticks = ctx.retry_ticks.get().saturating_add(1);
                ctx.retry_ticks.set(ticks);
                try_register_tray_icon(hwnd, ctx);
                if !ctx.added.get() && ticks >= TRAY_RETRY_MAX_TICKS {
                    // SAFETY: the retry budget is spent, so the timer is
                    // removed to stop it firing forever.
                    unsafe {
                        let _discarded = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
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
                    let _discarded = ctx.tx.send(TrayAction::ActivatePod);
                }
                WM_LBUTTONDBLCLK => {
                    let _discarded = ctx.tx.send(TrayAction::OpenChat);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    menu::show_context_menu(hwnd, ctx);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: `hwnd` is the window currently dispatching this
            // message; destroying it defers `WM_DESTROY`/`WM_NCDESTROY`.
            unsafe {
                ignore_teardown(DestroyWindow(hwnd));
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: the window is still alive during `WM_DESTROY`; its
            // retry timer must be cleared before that is no longer true.
            unsafe {
                let _discarded = KillTimer(Some(hwnd), TRAY_RETRY_TIMER_ID);
            }
            if ctx.added.get() {
                delete_tray_icon(hwnd);
                ctx.added.set(false);
            }
            ctx.resident.store(false, Ordering::Release);
            // SAFETY: posts to this thread's own queue, asking the
            // message loop above to stop; no state is borrowed.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: the window is being destroyed, so the slot is cleared
            // to prevent any later message from reading a freed pointer.
            unsafe {
                SetWindowLongPtrW(hwnd, GWL_USERDATA, 0);
            }
            // SAFETY: this is the single point where the box handed over at
            // `WM_NCCREATE` is reclaimed, and `WM_NCDESTROY` arrives once.
            let owned = unsafe { Box::from_raw(ptr) };
            if owned.icon_owned && !owned.icon.is_invalid() {
                // SAFETY: the icon is ours per `icon_owned` and is not used
                // again after the box is dropped.
                unsafe {
                    let _discarded = DestroyIcon(owned.icon);
                }
            }
            LRESULT(0)
        }
        // SAFETY: default handling for any message this procedure ignores.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
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
            Some(TrayAction::OpenSettings(TraySection::Companions))
        );
        assert_eq!(
            action_for_command(CMD_SETTINGS_CONNECTIONS),
            Some(TrayAction::OpenSettings(TraySection::Connections))
        );
        assert_eq!(
            action_for_command(CMD_SETTINGS_DEVICE),
            Some(TrayAction::OpenSettings(TraySection::Device))
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
