//! Owns the native tray icon on its own message-loop thread.

use super::{Tray, TrayAction, TrayHandle, TrayState, menu};
use parking_lot::RwLock;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::{BadIcon, Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconEvent};
use winit::{
    application::ApplicationHandler,
    event::{StartCause, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::windows::EventLoopBuilderExtWindows,
    window::WindowId,
};

const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: u8 = 31;

pub(super) enum Event {
    Refresh,
    Quit,
    Activate(TrayAction),
    Menu(tray_icon::menu::MenuEvent),
}

pub(super) fn spawn(initial: TrayState, tx: UnboundedSender<TrayAction>) -> Option<Tray> {
    let (ready, started) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("ene-tray".into())
        .spawn(move || run(initial, tx, ready))
        .ok()?;
    Some(Tray {
        handle: started.recv().ok()?,
        thread: Some(thread),
    })
}

fn run(initial: TrayState, tx: UnboundedSender<TrayAction>, ready: SyncSender<TrayHandle>) {
    let event_loop = EventLoop::<Event>::with_user_event()
        .with_any_thread(true)
        .with_dpi_aware(false)
        .build();
    let event_loop = match event_loop {
        Ok(event_loop) => event_loop,
        Err(error) => {
            tracing::warn!(%error, "system tray event loop unavailable");
            return;
        }
    };
    let handle = TrayHandle {
        shared: Arc::new(RwLock::new(initial)),
        events: event_loop.create_proxy(),
        resident: Arc::new(AtomicBool::new(false)),
    };
    let proxy = handle.events.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let action = match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => TrayAction::ActivatePod,
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => TrayAction::OpenChat,
            _ => return,
        };
        drop(proxy.send_event(Event::Activate(action)));
    }));
    let proxy = handle.events.clone();
    tray_icon::menu::MenuEvent::set_event_handler(Some(move |event| {
        drop(proxy.send_event(Event::Menu(event)));
    }));
    let mut tray = Worker {
        handle,
        tx,
        ready: Some(ready),
        icon: None,
        displayed: None,
        attempts: 0,
    };
    if let Err(error) = event_loop.run_app(&mut tray) {
        tracing::warn!(%error, "system tray event loop stopped");
    }
    tray.handle.resident.store(false, Ordering::Release);
}

struct Worker {
    handle: TrayHandle,
    tx: UnboundedSender<TrayAction>,
    ready: Option<SyncSender<TrayHandle>>,
    icon: Option<TrayIcon>,
    displayed: Option<TrayState>,
    attempts: u8,
}

impl Worker {
    fn install(&mut self, events: &ActiveEventLoop) {
        self.attempts += 1;
        let state = self.handle.shared.read().clone();
        let installed = install_icon(&state);
        match installed {
            Ok(icon) => {
                self.icon = Some(icon);
                self.displayed = Some(state);
                self.handle.resident.store(true, Ordering::Release);
                events.set_control_flow(ControlFlow::Wait);
            }
            Err(error) if self.attempts < MAX_ATTEMPTS => {
                tracing::debug!(%error, attempt = self.attempts, "system tray registration will retry");
                events.set_control_flow(ControlFlow::WaitUntil(Instant::now() + RETRY_INTERVAL));
            }
            Err(error) => {
                tracing::warn!(%error, "system tray registration failed");
                events.set_control_flow(ControlFlow::Wait);
            }
        }
    }

    fn refresh(&mut self) {
        let Some(icon) = &self.icon else { return };
        let state = self.handle.shared.read().clone();
        if self.displayed.as_ref() == Some(&state) {
            return;
        }
        let result = menu::build(&state).and_then(|menu| {
            icon.set_tooltip(Some(tooltip(&state.tooltip)))?;
            icon.set_menu(Some(Box::new(menu)));
            Ok(())
        });
        match result {
            Ok(()) => self.displayed = Some(state),
            Err(error) => tracing::warn!(%error, "could not refresh the system tray"),
        }
    }
}

impl ApplicationHandler<Event> for Worker {
    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(
            cause,
            StartCause::Init | StartCause::ResumeTimeReached { .. }
        ) && self.icon.is_none()
        {
            self.install(event_loop);
        }
        if let Some(ready) = self.ready.take() {
            drop(ready.send(self.handle.clone()));
        }
    }

    fn resumed(&mut self, _: &ActiveEventLoop) {}

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        let action = match event {
            Event::Refresh => {
                self.refresh();
                return;
            }
            Event::Quit => {
                event_loop.exit();
                return;
            }
            Event::Activate(action) => Some(action),
            Event::Menu(event) => menu::action(event.id.0.as_str()),
        };
        if let Some(action) = action
            && self.tx.send(action).is_err()
        {
            event_loop.exit();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum TrayError {
    #[error(transparent)]
    Icon(#[from] BadIcon),
    #[error(transparent)]
    Menu(#[from] tray_icon::menu::Error),
    #[error(transparent)]
    Registration(#[from] tray_icon::Error),
}

fn install_icon(state: &TrayState) -> Result<TrayIcon, TrayError> {
    let icon = Icon::from_resource(1, None).or_else(|_| fallback_icon())?;
    Ok(TrayIcon::builder()
        .with_icon(icon)
        .with_tooltip(tooltip(&state.tooltip))
        .with_menu(Box::new(menu::build(state)?))
        .with_menu_on_left_click(false)
        .build()?)
}

fn fallback_icon() -> Result<Icon, BadIcon> {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let center = (SIZE as f32 - 1.0) * 0.5;
    let radius = SIZE as f32 * 0.45;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = (x as f32 - center) / radius;
            let dy = (y as f32 - center) / radius;
            let color = if dx.hypot(dy) > 1.0 {
                [0, 0, 0, 0]
            } else if dx.abs().sqrt() + dy.abs().sqrt() <= 0.85 {
                [255, 253, 253, 255]
            } else {
                [207, 46, 104, 255]
            };
            rgba.extend_from_slice(&color);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}

// Reserve one UTF-16 code unit for Shell_NotifyIcon's terminating NUL.
fn tooltip(text: &str) -> &str {
    let mut units = 0;
    for (offset, ch) in text.char_indices() {
        units += ch.len_utf16();
        if units > 127 || ch == '\0' {
            return &text[..offset];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::tooltip;

    #[test]
    fn tooltip_keeps_codepoints_whole_and_leaves_room_for_the_terminator() {
        let prefix = "a".repeat(126);
        assert_eq!(tooltip(&format!("{prefix}😀")), prefix);
        let fits = format!("{}😀", "a".repeat(125));
        assert_eq!(tooltip(&fits), fits);
    }
}
