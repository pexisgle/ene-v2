mod protocol;

use self::protocol::{parse_wm_class, wayland_state_is_activated};
use crate::{Observation, Window};
use parking_lot::Mutex;
use std::{collections::HashMap, sync::LazyLock, time::Instant};
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

static BACKEND: LazyLock<Mutex<LinuxBackend>> = LazyLock::new(|| Mutex::new(init()));

enum LinuxBackend {
    Wayland(Box<WaylandBackend>),
    X11(Box<X11Backend>),
    Unsupported,
}

pub fn supported() -> bool {
    let backend = BACKEND.lock();
    !matches!(*backend, LinuxBackend::Unsupported)
}

pub fn observe() -> Observation {
    let mut backend = BACKEND.lock();
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
        return WaylandBackend::connect().map_or(LinuxBackend::Unsupported, |backend| {
            LinuxBackend::Wayland(Box::new(backend))
        });
    }
    if std::env::var_os("DISPLAY").is_some_and(|val| !val.is_empty()) {
        return X11Backend::connect().map_or(LinuxBackend::Unsupported, |backend| {
            LinuxBackend::X11(Box::new(backend))
        });
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
        let _flushed = self.queue.flush();
        let _dispatched = self.queue.dispatch_pending(&mut self.state);
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    let _dispatched = self.queue.dispatch_pending(&mut self.state);
                }
                Err(wayland_client::backend::WaylandError::Io(err))
                    if err.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
        } else {
            let _dispatched = self.queue.dispatch_pending(&mut self.state);
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
