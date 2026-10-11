//! Win32 notification-area icon with Explorer restart recovery and bounded hide leases.
use crate::AppWindow;
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Shell::*, WindowsAndMessaging::*},
    },
    core::{PCWSTR, w},
};
pub const LEASE: Duration = Duration::from_secs(2);
pub struct Registration {
    pub checked_at: Instant,
    pub registered: bool,
}
impl Registration {
    pub fn usable_at(&self, now: Instant) -> bool {
        self.registered && now.saturating_duration_since(self.checked_at) < LEASE
    }
}
type Callback = RefCell<Box<dyn Fn()>>;
pub struct Ui {
    actions: [Callback; 7],
    state: Arc<Mutex<(bool, bool, String)>>,
    stop: Arc<AtomicBool>,
    worker: RefCell<Option<std::thread::JoinHandle<()>>>,
}
macro_rules! callback {
    ($name:ident, $index:expr) => {
        pub fn $name(&self, cb: impl Fn() + 'static) {
            *self.actions[$index].borrow_mut() = Box::new(cb);
        }
    };
}
impl Ui {
    callback!(on_show_chat, 0);
    callback!(on_open_settings, 1);
    callback!(on_toggle_avatar, 2);
    callback!(on_recenter_avatar, 3);
    callback!(on_restart_avatar, 4);
    callback!(on_quit, 5);
    callback!(on_open_about, 6);
    pub fn set_connection_status(&self, status: &str) {
        self.state.lock().unwrap().2 = status.into();
    }
    pub fn set_avatar_visible(&self, value: bool) {
        self.state.lock().unwrap().0 = value;
    }
    pub fn set_avatar_failed(&self, value: bool) {
        self.state.lock().unwrap().1 = value;
    }
    pub fn new(chat: slint::Weak<AppWindow>) -> (Rc<Self>, Receiver<Registration>) {
        let state = Arc::new(Mutex::new((true, false, "ホスト未接続".into())));
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker_chat = chat.clone();
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            if let Err(error) = serve(worker_chat, worker_state, worker_stop, sender) {
                eprintln!("tray: {error}");
            }
        });
        let ui = Rc::new(Self {
            actions: std::array::from_fn(|_| RefCell::new(Box::new(|| {}) as Box<dyn Fn()>)),
            state,
            stop,
            worker: RefCell::new(Some(worker)),
        });
        let weak = Rc::downgrade(&ui);
        if let Some(chat) = chat.upgrade() {
            chat.on_tray_action(move |index| {
                if let Some(ui) = weak.upgrade()
                    && let Some(cb) = ui.actions.get(index as usize)
                {
                    cb.borrow()();
                }
            });
        }
        (ui, receiver)
    }
}
impl Drop for Ui {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.get_mut().take() {
            let _ = worker.join();
        }
    }
}
struct Context {
    chat: slint::Weak<AppWindow>,
    state: Arc<Mutex<(bool, bool, String)>>,
    stop: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Registration>,
    data: Cell<NOTIFYICONDATAW>,
    taskbar: u32,
    registered: Cell<bool>,
}
impl Context {
    fn action(&self, index: i32) {
        let _ = self
            .chat
            .upgrade_in_event_loop(move |ui| ui.invoke_tray_action(index));
    }
    unsafe fn register(&self) {
        let mut data = self.data.get();
        self.registered
            .set(unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool());
        if self.registered.get() {
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            unsafe {
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
            }
            self.data.set(data);
        }
    }
    unsafe fn menu(&self, hwnd: HWND) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        let (visible, failed, _status) = self.state.lock().unwrap().clone();
        for (i, label) in [
            "対話を表示",
            "設定",
            if visible {
                "キャラクターを隠す"
            } else {
                "キャラクターを表示"
            },
            "キャラクターを画面中央へ戻す",
            "描画エラーから再起動",
            "ホスト選択・終了",
            "このアプリについて",
        ]
        .iter()
        .enumerate()
        {
            let text: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
            let flags = if i == 4 && !failed {
                MF_STRING | MF_GRAYED
            } else {
                MF_STRING
            };
            let _ = unsafe { AppendMenuW(menu, flags, i + 1, PCWSTR(text.as_ptr())) };
        }
        let mut point = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
            let _ = SetForegroundWindow(hwnd);
        }
        let selected = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            )
        }
        .0;
        if (1..=6).contains(&selected) {
            self.action(selected - 1);
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }
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
        match message {
            WM_TIMER => {
                if context.stop.load(Ordering::Acquire) {
                    unsafe {
                        let _ = EndMenu();
                        let _ = DestroyWindow(hwnd);
                    }
                    return LRESULT(0);
                }
                if context.registered.get() {
                    context.registered.set(
                        unsafe { Shell_NotifyIconW(NIM_MODIFY, &context.data.get()) }.as_bool(),
                    );
                }
                if !context.registered.get() {
                    unsafe {
                        context.register();
                    }
                }
                let _ = context.sender.try_send(Registration {
                    checked_at: Instant::now(),
                    registered: context.registered.get(),
                });
                return LRESULT(0);
            }
            WM_APP => {
                match lp.0 as u32 & 0xffff {
                    WM_LBUTTONUP | NIN_SELECT | 0x401 => context.action(0),
                    WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                        context.menu(hwnd);
                    },
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_DESTROY => {
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &context.data.get());
                    PostQuitMessage(0);
                }
                return LRESULT(0);
            }
            m if m == context.taskbar => {
                context.registered.set(false);
                unsafe {
                    context.register();
                }
                return LRESULT(0);
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wp, lp) }
}
fn serve(
    chat: slint::Weak<AppWindow>,
    state: Arc<Mutex<(bool, bool, String)>>,
    stop: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Registration>,
) -> windows::core::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("ene.tray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let mut context = Box::new(Context {
            chat,
            state,
            stop,
            sender,
            data: Cell::new(NOTIFYICONDATAW::default()),
            taskbar: RegisterWindowMessageW(w!("TaskbarCreated")),
            registered: Cell::new(false),
        });
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class,
            w!("ene"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            Some((&mut *context as *mut Context).cast()),
        )?;
        let mut data = context.data.get();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = 1;
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_APP;
        data.hIcon = LoadIconW(None, IDI_APPLICATION)?;
        data.szTip[..3].copy_from_slice(&[101, 110, 101]);
        context.data.set(data);
        context.register();
        if SetTimer(Some(hwnd), 1, 500, None) == 0 {
            let _ = DestroyWindow(hwnd);
            return Err(windows::core::Error::from_win32());
        }
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        let _ = UnregisterClassW(class, Some(instance.into()));
        Ok(())
    }
}
