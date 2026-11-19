//! Desktop observation and local `eye.watch` rules.

use serde::Deserialize;
use serde_json::Value;
use std::{
    io::{self, Write},
    path::PathBuf,
};

#[cfg(windows)]
pub(crate) mod screen;
#[cfg(windows)]
pub(crate) use screen::{capture, unlocked};

fn preference_path(name: &str) -> io::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "ene")
        .ok_or_else(|| io::Error::other("設定ディレクトリを取得できません"))?;
    Ok(dirs.config_local_dir().join(name))
}

pub(crate) fn enabled() -> bool {
    preference_enabled("desktop-eyes")
}

pub(crate) fn preference_enabled(name: &str) -> bool {
    preference_path(name)
        .and_then(std::fs::read)
        .is_ok_and(|value| value == b"on")
}

pub(crate) fn save_enabled(enabled: bool) -> io::Result<()> {
    save_preference("desktop-eyes", enabled)
}

pub(crate) fn save_preference(name: &str, enabled: bool) -> io::Result<()> {
    let path = preference_path(name)?;
    let directory = path.parent().expect("a preference path has a directory");
    std::fs::create_dir_all(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(if enabled { b"on" } else { b"off" })?;
    file.persist(path)?;
    Ok(())
}

/// What the sensor can observe right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// The foreground window's title and process name, when readable.
    pub window: Option<Window>,
    /// Seconds since the last input, and whether the user counts as active.
    pub idle_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub title: String,
    pub app_id: String,
}

/// A parsed `eye.watch` rule.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    pub target_pattern: String,
    pub match_kind: MatchKind,
    pub notify_on: NotifyOn,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Title,
    AppId,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotifyOn {
    Focus,
    Blur,
    TitleChange,
}

impl Watch {
    /// Parses a rule from a `device_request` or `set_eye_watch` input.
    pub fn parse(input: &Value) -> Result<Self, String> {
        serde_json::from_value(input.clone()).map_err(|error| error.to_string())
    }

    /// Whether the observed window matches this rule's target.
    ///
    /// `target_pattern` is documented as "a substring or regular expression"
    /// (`docs/api.md` §知覚), so it is compiled as a case-insensitive pattern
    /// and falls back to a case-insensitive substring when it is not valid
    /// regex. A pattern that matches nothing is the model's mistake, not a
    /// reason to fail the operation.
    pub fn matches(&self, window: &Window) -> bool {
        let subject = match self.match_kind {
            MatchKind::Title => &window.title,
            MatchKind::AppId => &window.app_id,
        };
        match regex::RegexBuilder::new(&self.target_pattern)
            .case_insensitive(true)
            .build()
        {
            Ok(pattern) => pattern.is_match(subject),
            Err(_) => subject
                .to_lowercase()
                .contains(&self.target_pattern.to_lowercase()),
        }
    }
}

/// The sensor's state between samples: what the last observation was, so a
/// rule can fire on a transition rather than on every sample.
#[derive(Default)]
pub struct Sensor {
    /// The rule currently applied by the host, if any.
    pub watch: Option<Watch>,
    /// The window the last sample saw, for focus and title-change edges.
    last: Option<Window>,
    /// Whether the last sample matched, so `focus` fires once per entry.
    matched: bool,
    idle: bool,
}

/// What a sample produced for the host.
pub enum Report {
    /// The foreground window changed.
    Window(Window),
    /// The user's activity state changed.
    Activity { is_active: bool, idle_seconds: u64 },
    /// A watch rule matched on this transition.
    Interrupt(Window, NotifyOn),
}

impl Sensor {
    /// Applies a rule the host dispatched. `None` clears it.
    pub fn apply(&mut self, watch: Option<Watch>) {
        self.watch = watch;
        self.matched = false;
    }

    /// Turns one observation into the reports the host should receive.
    ///
    /// A changed window is always reported, because the host summarizes it for
    /// the conversation. A matching rule adds an interrupt, so the parent is
    /// woken without waiting for the next periodic sample.
    pub fn observe(&mut self, observation: Observation) -> Vec<Report> {
        let mut reports = Vec::new();
        if let Some(window) = observation.window {
            let changed = self.last.as_ref() != Some(&window);
            if changed {
                reports.push(Report::Window(window.clone()));
            }
            if let Some(watch) = self.watch.as_ref().filter(|watch| watch.enabled) {
                let matched = watch.matches(&window);
                let fires = match watch.notify_on {
                    NotifyOn::Focus => matched && !self.matched,
                    NotifyOn::Blur => !matched && self.matched,
                    NotifyOn::TitleChange => {
                        matched
                            && self.last.as_ref().is_some_and(|last| {
                                last.app_id == window.app_id && last.title != window.title
                            })
                    }
                };
                if fires {
                    let target = match watch.notify_on {
                        NotifyOn::Blur => self.last.as_ref().unwrap_or(&window),
                        _ => &window,
                    };
                    reports.push(Report::Interrupt(target.clone(), watch.notify_on));
                }
                self.matched = matched;
            }
            self.last = Some(window);
        }
        let idle = observation.idle_seconds >= IDLE_THRESHOLD;
        if idle != self.idle {
            reports.push(Report::Activity {
                is_active: !idle,
                idle_seconds: observation.idle_seconds,
            });
        }
        self.idle = idle;
        reports
    }
}

/// The idle threshold the design names for the activity event.
pub const IDLE_THRESHOLD: u64 = 60;

/// Whether this platform can report the foreground window at all.
///
/// A platform without the hook declares no `eye.watch`, so the host never
/// publishes a tool this client cannot honour.
pub fn supported() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        linux_impl::supported()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// Samples the OS once.
#[cfg(windows)]
pub fn observe() -> Observation {
    windows_impl::observe()
}

/// Samples the OS once on Linux (Wayland `wlr-foreign-toplevel-management`
/// or X11 `_NET_ACTIVE_WINDOW`), falling back safely when unavailable.
#[cfg(target_os = "linux")]
pub fn observe() -> Observation {
    linux_impl::observe()
}

/// A platform without a window hook reports no window, and says so by
/// returning `None` rather than inventing one.
#[cfg(not(any(windows, target_os = "linux")))]
pub fn observe() -> Observation {
    Observation {
        window: None,
        idle_seconds: 0,
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse_wm_class(bytes: &[u8]) -> String {
    let mut parts = bytes
        .split(|&byte| byte == 0)
        .map(|part| String::from_utf8_lossy(part).into_owned());
    let instance = parts.next().unwrap_or_default();
    let class = parts.next().unwrap_or_default();
    if !class.is_empty() { class } else { instance }
}

#[cfg(any(target_os = "linux", test))]
fn wayland_state_is_activated(bytes: &[u8]) -> bool {
    const ACTIVATED: u32 = 2;
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks
        .iter()
        .any(|chunk| u32::from_ne_bytes(*chunk) == ACTIVATED)
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{Observation, Window, parse_wm_class, wayland_state_is_activated};
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
        time::Instant,
    };
    use wayland_client::backend::ObjectId;
    use wayland_client::{
        Connection as WaylandConnection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
        event_created_child,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_registry, wl_seat},
    };
    use wayland_protocols::ext::idle_notify::v1::client::{
        ext_idle_notification_v1, ext_idle_notifier_v1,
    };
    use wayland_protocols_wlr::foreign_toplevel::v1::client::{
        zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
    };
    use x11rb::{
        connection::Connection as _,
        protocol::screensaver,
        protocol::xproto::{self, ConnectionExt as _},
        rust_connection::RustConnection,
    };

    const _: () = assert!(zwlr_foreign_toplevel_handle_v1::State::Activated as u32 == 2);

    static BACKEND: OnceLock<Mutex<LinuxBackend>> = OnceLock::new();

    enum LinuxBackend {
        Wayland(Box<WaylandBackend>),
        X11(Box<X11Backend>),
        Unsupported,
    }

    pub(super) fn supported() -> bool {
        let Ok(backend) = BACKEND.get_or_init(|| Mutex::new(init())).lock() else {
            return false;
        };
        !matches!(*backend, LinuxBackend::Unsupported)
    }

    pub(super) fn observe() -> Observation {
        let Ok(mut backend) = BACKEND.get_or_init(|| Mutex::new(init())).lock() else {
            return empty();
        };
        match &mut *backend {
            LinuxBackend::Wayland(wayland) => wayland.observe(),
            LinuxBackend::X11(x11) => x11.observe(),
            LinuxBackend::Unsupported => empty(),
        }
    }

    fn empty() -> Observation {
        Observation {
            window: None,
            idle_seconds: 0,
        }
    }

    fn init() -> LinuxBackend {
        // Under Wayland, never fall back to XWayland's `_NET_ACTIVE_WINDOW`:
        // it cannot observe native Wayland surfaces.
        if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|val| !val.is_empty()) {
            return WaylandBackend::connect()
                .map(|backend| LinuxBackend::Wayland(Box::new(backend)))
                .unwrap_or(LinuxBackend::Unsupported);
        }
        if std::env::var_os("DISPLAY").is_some_and(|val| !val.is_empty()) {
            return X11Backend::connect()
                .map(|backend| LinuxBackend::X11(Box::new(backend)))
                .unwrap_or(LinuxBackend::Unsupported);
        }
        LinuxBackend::Unsupported
    }

    #[derive(Clone, Default)]
    struct ToplevelSnapshot {
        title: String,
        app_id: String,
        activated: bool,
    }

    #[derive(Default)]
    struct ToplevelEntry {
        pending: ToplevelSnapshot,
        committed: ToplevelSnapshot,
        handle: Option<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1>,
    }

    #[derive(Default)]
    struct WaylandState {
        toplevels: HashMap<ObjectId, ToplevelEntry>,
        active_id: Option<ObjectId>,
        idled_at: Option<Instant>,
        finished: bool,
    }

    impl WaylandState {
        fn active_window(&self) -> Option<Window> {
            let entry = self
                .active_id
                .as_ref()
                .and_then(|id| self.toplevels.get(id))
                .filter(|entry| entry.committed.activated)
                .or_else(|| {
                    self.toplevels
                        .values()
                        .find(|entry| entry.committed.activated)
                })?;
            if entry.committed.title.is_empty() && entry.committed.app_id.is_empty() {
                return None;
            }
            Some(Window {
                title: entry.committed.title.clone(),
                app_id: entry.committed.app_id.clone(),
            })
        }

        fn refresh_active(&mut self) {
            if self.active_id.as_ref().is_some_and(|id| {
                self.toplevels
                    .get(id)
                    .is_some_and(|e| e.committed.activated)
            }) {
                return;
            }
            self.active_id = self
                .toplevels
                .iter()
                .find_map(|(id, entry)| entry.committed.activated.then(|| id.clone()));
        }
    }

    struct WaylandBackend {
        queue: EventQueue<WaylandState>,
        state: WaylandState,
        _manager: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
        _seat: Option<wl_seat::WlSeat>,
        _idle_notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1>,
        _idle_notification: Option<ext_idle_notification_v1::ExtIdleNotificationV1>,
    }

    impl WaylandBackend {
        fn connect() -> Option<Self> {
            let conn = WaylandConnection::connect_to_env().ok()?;
            let (globals, mut queue) = registry_queue_init::<WaylandState>(&conn).ok()?;
            let qh = queue.handle();
            let manager: zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1 =
                globals.bind(&qh, 1..=3, ()).ok()?;

            let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=9, ()).ok();
            let idle_notifier = globals
                .bind::<ext_idle_notifier_v1::ExtIdleNotifierV1, _, _>(&qh, 1..=2, ())
                .ok();
            let idle_notification = match (&seat, &idle_notifier) {
                (Some(seat), Some(notifier)) => {
                    if notifier.version() >= 2 {
                        Some(notifier.get_input_idle_notification(1_000, seat, &qh, ()))
                    } else {
                        Some(notifier.get_idle_notification(1_000, seat, &qh, ()))
                    }
                }
                _ => None,
            };

            let mut state = WaylandState::default();
            queue.roundtrip(&mut state).ok()?;
            queue.roundtrip(&mut state).ok()?;

            Some(Self {
                queue,
                state,
                _manager: manager,
                _seat: seat,
                _idle_notifier: idle_notifier,
                _idle_notification: idle_notification,
            })
        }

        fn observe(&mut self) -> Observation {
            self.pump();
            if self.state.finished {
                return empty();
            }
            Observation {
                window: self.state.active_window(),
                idle_seconds: self
                    .state
                    .idled_at
                    .map_or(0, |at| 1 + at.elapsed().as_secs()),
            }
        }

        fn pump(&mut self) {
            let _ = self.queue.flush();
            let _ = self.queue.dispatch_pending(&mut self.state);
            if let Some(guard) = self.queue.prepare_read() {
                match guard.read() {
                    Ok(_) => {
                        let _ = self.queue.dispatch_pending(&mut self.state);
                    }
                    Err(wayland_client::backend::WaylandError::Io(err))
                        if err.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {}
                }
            } else {
                let _ = self.queue.dispatch_pending(&mut self.state);
            }
        }
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
        fn event(
            _state: &mut Self,
            _proxy: &wl_registry::WlRegistry,
            _event: wl_registry::Event,
            _data: &GlobalListContents,
            _conn: &WaylandConnection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
    delegate_noop!(WaylandState: ignore ext_idle_notifier_v1::ExtIdleNotifierV1);

    impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for WaylandState {
        fn event(
            state: &mut Self,
            _proxy: &ext_idle_notification_v1::ExtIdleNotificationV1,
            event: ext_idle_notification_v1::Event,
            _data: &(),
            _conn: &WaylandConnection,
            _qh: &QueueHandle<Self>,
        ) {
            match event {
                ext_idle_notification_v1::Event::Idled => {
                    state.idled_at.get_or_insert_with(Instant::now);
                }
                ext_idle_notification_v1::Event::Resumed => {
                    state.idled_at = None;
                }
                _ => {}
            }
        }
    }

    impl Dispatch<zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1, ()> for WaylandState {
        fn event(
            state: &mut Self,
            _proxy: &zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
            event: zwlr_foreign_toplevel_manager_v1::Event,
            _data: &(),
            _conn: &WaylandConnection,
            _qh: &QueueHandle<Self>,
        ) {
            match event {
                zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } => {
                    let id = toplevel.id();
                    state.toplevels.insert(
                        id,
                        ToplevelEntry {
                            pending: ToplevelSnapshot::default(),
                            committed: ToplevelSnapshot::default(),
                            handle: Some(toplevel),
                        },
                    );
                }
                zwlr_foreign_toplevel_manager_v1::Event::Finished => {
                    state.finished = true;
                    for entry in state.toplevels.values() {
                        if let Some(handle) = &entry.handle {
                            handle.destroy();
                        }
                    }
                    state.toplevels.clear();
                    state.active_id = None;
                }
                _ => {}
            }
        }

        event_created_child!(
            WaylandState,
            zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1,
            [
                zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE =>
                    (zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()),
            ]
        );
    }

    impl Dispatch<zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1, ()> for WaylandState {
        fn event(
            state: &mut Self,
            proxy: &zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1,
            event: zwlr_foreign_toplevel_handle_v1::Event,
            _data: &(),
            _conn: &WaylandConnection,
            _qh: &QueueHandle<Self>,
        ) {
            let id = proxy.id();
            match event {
                zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                    if let Some(entry) = state.toplevels.get_mut(&id) {
                        entry.pending.title = title;
                    }
                }
                zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                    if let Some(entry) = state.toplevels.get_mut(&id) {
                        entry.pending.app_id = app_id;
                    }
                }
                zwlr_foreign_toplevel_handle_v1::Event::State { state: raw } => {
                    if let Some(entry) = state.toplevels.get_mut(&id) {
                        entry.pending.activated = wayland_state_is_activated(&raw);
                    }
                }
                zwlr_foreign_toplevel_handle_v1::Event::Done => {
                    if let Some(entry) = state.toplevels.get_mut(&id) {
                        entry.committed = entry.pending.clone();
                        if entry.committed.activated {
                            state.active_id = Some(id);
                        } else if state.active_id.as_ref() == Some(&id) {
                            state.active_id = None;
                            state.refresh_active();
                        }
                    }
                }
                zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                    state.toplevels.remove(&id);
                    if state.active_id.as_ref() == Some(&id) {
                        state.active_id = None;
                        state.refresh_active();
                    }
                    proxy.destroy();
                }
                _ => {}
            }
        }
    }

    struct X11Backend {
        conn: RustConnection,
        root: xproto::Window,
        net_active_window: xproto::Atom,
        net_wm_name: xproto::Atom,
        utf8_string: xproto::Atom,
    }

    impl X11Backend {
        fn connect() -> Option<Self> {
            let (conn, screen_num) = RustConnection::connect(None).ok()?;
            let root = conn.setup().roots.get(screen_num)?.root;
            let net_supported = conn
                .intern_atom(false, b"_NET_SUPPORTED")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let net_active_window = conn
                .intern_atom(false, b"_NET_ACTIVE_WINDOW")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let net_wm_name = conn
                .intern_atom(false, b"_NET_WM_NAME")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let utf8_string = conn
                .intern_atom(false, b"UTF8_STRING")
                .ok()?
                .reply()
                .ok()?
                .atom;

            let supported = conn
                .get_property(
                    false,
                    root,
                    net_supported,
                    xproto::AtomEnum::ATOM,
                    0,
                    u32::MAX,
                )
                .ok()?
                .reply()
                .ok()?;
            let supports_active = supported
                .value32()
                .is_some_and(|mut atoms| atoms.any(|atom| atom == net_active_window));
            if !supports_active {
                return None;
            }
            Some(Self {
                conn,
                root,
                net_active_window,
                net_wm_name,
                utf8_string,
            })
        }

        fn observe(&self) -> Observation {
            let window = self.foreground();
            let idle_seconds = screensaver::query_info(&self.conn, self.root)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or(0, |info| u64::from(info.ms_since_user_input) / 1000);
            Observation {
                window,
                idle_seconds,
            }
        }

        fn foreground(&self) -> Option<Window> {
            let reply = self
                .conn
                .get_property(
                    false,
                    self.root,
                    self.net_active_window,
                    xproto::AtomEnum::WINDOW,
                    0,
                    1,
                )
                .ok()?
                .reply()
                .ok()?;
            let active = reply.value32().and_then(|mut it| it.next()).unwrap_or(0);
            if active == 0 {
                return None;
            }
            let title = self.window_title(active);
            let app_id = self.window_class(active);
            if title.is_empty() && app_id.is_empty() {
                return None;
            }
            Some(Window { title, app_id })
        }

        fn window_title(&self, window: xproto::Window) -> String {
            if let Ok(cookie) =
                self.conn
                    .get_property(false, window, self.net_wm_name, self.utf8_string, 0, 1024)
                && let Ok(reply) = cookie.reply()
                && !reply.value.is_empty()
                && let Ok(text) = std::str::from_utf8(&reply.value)
            {
                return text.trim_end_matches('\0').to_owned();
            }
            if let Ok(cookie) = self.conn.get_property(
                false,
                window,
                xproto::AtomEnum::WM_NAME,
                xproto::AtomEnum::ANY,
                0,
                1024,
            ) && let Ok(reply) = cookie.reply()
                && !reply.value.is_empty()
            {
                return String::from_utf8_lossy(&reply.value)
                    .trim_end_matches('\0')
                    .to_owned();
            }
            String::new()
        }

        fn window_class(&self, window: xproto::Window) -> String {
            let Ok(cookie) = self.conn.get_property(
                false,
                window,
                xproto::AtomEnum::WM_CLASS,
                xproto::AtomEnum::STRING,
                0,
                1024,
            ) else {
                return String::new();
            };
            let Ok(reply) = cookie.reply() else {
                return String::new();
            };
            parse_wm_class(&reply.value)
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    use super::{Observation, Window};
    use windows::Win32::{
        Foundation::{CloseHandle, HWND, MAX_PATH},
        System::Threading::{
            OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW,
        },
        UI::{
            Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
            WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId},
        },
    };

    pub(super) fn observe() -> Observation {
        Observation {
            window: foreground(),
            idle_seconds: idle_seconds(),
        }
    }

    /// The foreground window's title and process name. Either may be empty:
    /// a window the OS will not describe is reported as it is, not guessed.
    fn foreground() -> Option<Window> {
        // SAFETY: the calls read OS state into locals; every handle is closed.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd == HWND::default() {
                return None;
            }
            let mut buffer = [0u16; MAX_PATH as usize];
            let length = GetWindowTextW(hwnd, &mut buffer);
            let title = String::from_utf16_lossy(&buffer[..length.max(0) as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let app_id = process_name(pid).unwrap_or_default();
            if title.is_empty() && app_id.is_empty() {
                return None;
            }
            Some(Window { title, app_id })
        }
    }

    /// The executable's file name, which is the app id a rule matches on.
    fn process_name(pid: u32) -> Option<String> {
        // SAFETY: the handle is closed on every path below.
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = [0u16; MAX_PATH as usize];
            let mut size = buffer.len() as u32;
            let result = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut size,
            );
            let _ = CloseHandle(handle);
            result.ok()?;
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            path.rsplit(['\\', '/']).next().map(ToOwned::to_owned)
        }
    }

    fn idle_seconds() -> u64 {
        // SAFETY: `LASTINPUTINFO` is a plain struct filled by the OS.
        unsafe {
            let mut info = LASTINPUTINFO {
                cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                dwTime: 0,
            };
            if !GetLastInputInfo(&mut info).as_bool() {
                return 0;
            }
            let now = windows::Win32::System::SystemInformation::GetTickCount();
            u64::from(now.wrapping_sub(info.dwTime)) / 1000
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(title: &str, app_id: &str) -> Window {
        Window {
            title: title.into(),
            app_id: app_id.into(),
        }
    }

    fn rule(match_kind: MatchKind, notify_on: NotifyOn) -> Watch {
        Watch {
            target_pattern: "GitHub".into(),
            match_kind,
            notify_on,
            enabled: true,
        }
    }

    #[test]
    fn a_rule_matches_a_substring_of_its_chosen_subject() {
        let by_title = rule(MatchKind::Title, NotifyOn::Focus);
        assert!(by_title.matches(&window("GitHub - ene", "chrome.exe")));
        assert!(!by_title.matches(&window("editor", "github.exe")));
        let by_app = rule(MatchKind::AppId, NotifyOn::Focus);
        assert!(by_app.matches(&window("anything", "github.exe")));
        assert!(!by_app.matches(&window("GitHub", "chrome.exe")));
    }

    /// The declared schema promises a regular expression, and the design's own
    /// example is `.*\.rs`.
    #[test]
    fn a_rule_accepts_a_regular_expression() {
        let rust_file = Watch {
            target_pattern: r".*\.rs".into(),
            match_kind: MatchKind::Title,
            notify_on: NotifyOn::Focus,
            enabled: true,
        };
        assert!(rust_file.matches(&window("main.rs - ene-v2", "code.exe")));
        assert!(!rust_file.matches(&window("README.md", "code.exe")));
    }

    /// A pattern that is not valid regex still works as a substring instead of
    /// matching nothing.
    #[test]
    fn an_invalid_pattern_falls_back_to_a_substring() {
        let unbalanced = Watch {
            target_pattern: "a(b".into(),
            match_kind: MatchKind::Title,
            notify_on: NotifyOn::Focus,
            enabled: true,
        };
        assert!(unbalanced.matches(&window("x a(b y", "app")));
        assert!(!unbalanced.matches(&window("nothing", "app")));
    }

    #[test]
    fn a_rule_parses_only_from_a_complete_declaration() {
        let complete = serde_json::json!({
            "target_pattern": "GitHub",
            "match_kind": "title",
            "notify_on": "focus",
            "enabled": true,
        });
        assert!(Watch::parse(&complete).is_ok());
        let mut incomplete = complete.clone();
        incomplete.as_object_mut().unwrap().remove("notify_on");
        assert!(Watch::parse(&incomplete).is_err());
        let mut unknown = complete;
        unknown["extra"] = serde_json::json!(1);
        assert!(Watch::parse(&unknown).is_err());
    }

    /// The parent must be woken on the transition into a match, not on every
    /// sample while the match holds.
    #[test]
    fn a_focus_rule_fires_once_per_entry() {
        let mut sensor = Sensor::default();
        sensor.apply(Some(rule(MatchKind::Title, NotifyOn::Focus)));
        let github = Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        };
        let other = Observation {
            window: Some(window("editor", "code.exe")),
            idle_seconds: 0,
        };
        assert!(matches!(
            sensor.observe(other).as_slice(),
            [Report::Window(_)]
        ));
        let first = sensor.observe(github.clone());
        assert!(
            first
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_, _))),
            "entering the match interrupts"
        );
        let second = sensor.observe(github);
        assert!(
            !second
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_, _))),
            "staying in the match does not interrupt again"
        );
    }

    #[test]
    fn a_blur_rule_fires_when_the_match_is_left() {
        let mut sensor = Sensor::default();
        sensor.apply(Some(rule(MatchKind::Title, NotifyOn::Blur)));
        let github = Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        };
        let other = Observation {
            window: Some(window("editor", "code.exe")),
            idle_seconds: 0,
        };
        sensor.observe(github);
        let left = sensor.observe(other);
        assert!(
            left.iter()
                .any(|report| matches!(report, Report::Interrupt(window, NotifyOn::Blur) if window.title == "GitHub")),
            "leaving the match interrupts"
        );
    }

    #[test]
    fn a_disabled_rule_never_interrupts() {
        let mut sensor = Sensor::default();
        let mut disabled = rule(MatchKind::Title, NotifyOn::Focus);
        disabled.enabled = false;
        sensor.apply(Some(disabled));
        let reports = sensor.observe(Observation {
            window: Some(window("GitHub", "chrome.exe")),
            idle_seconds: 0,
        });
        assert!(
            !reports
                .iter()
                .any(|report| matches!(report, Report::Interrupt(_, _)))
        );
    }

    #[test]
    fn an_idle_observation_reports_the_activity_state() {
        let mut sensor = Sensor::default();
        let reports = sensor.observe(Observation {
            window: None,
            idle_seconds: IDLE_THRESHOLD,
        });
        assert!(matches!(
            reports.as_slice(),
            [Report::Activity {
                is_active: false,
                ..
            }]
        ));
        assert!(matches!(
            sensor
                .observe(Observation {
                    window: None,
                    idle_seconds: 0
                })
                .as_slice(),
            [Report::Activity {
                is_active: true,
                ..
            }]
        ));
    }

    #[test]
    fn x11_wm_class_prefers_class_and_falls_back_to_instance() {
        assert_eq!(parse_wm_class(b"kitty\0kitty\0"), "kitty");
        assert_eq!(parse_wm_class(b"navigator\0firefox\0"), "firefox");
        assert_eq!(parse_wm_class(b"only-instance\0\0"), "only-instance");
        assert_eq!(parse_wm_class(b""), "");
    }

    #[test]
    fn wayland_toplevel_state_detects_activated_entry() {
        let inactive = [0u32, 1u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        assert!(!wayland_state_is_activated(&inactive));

        let active = [0u32, 2u32, 3u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        assert!(wayland_state_is_activated(&active));

        // Truncated trailing bytes never trigger false activation or panic.
        assert!(!wayland_state_is_activated(&[2, 0, 0]));
    }
}
