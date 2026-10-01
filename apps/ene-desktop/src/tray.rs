//! Slint 1.18.1 does not expose native tray registration failures. Verify that
//! THIS process's item is registered with the session's StatusNotifierWatcher.
//! Merely finding a watcher would allow an unreachable hidden window.
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

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

pub fn monitor() -> Receiver<Registration> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        loop {
            // Start the lease BEFORE the probe so a delayed reply cannot renew it.
            let checked_at = Instant::now();
            let registered = probe().unwrap_or(false);
            if matches!(
                sender.try_send(Registration {
                    checked_at,
                    registered,
                }),
                Err(mpsc::TrySendError::Disconnected(_))
            ) {
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    receiver
}

fn probe() -> zbus::Result<bool> {
    let connection = zbus::blocking::connection::Builder::session()?
        .method_timeout(Duration::from_millis(500))
        .build()?;
    let watcher = zbus::blocking::Proxy::new(
        &connection,
        "org.kde.StatusNotifierWatcher",
        "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    if !watcher.get_property::<bool>("IsStatusNotifierHostRegistered")? {
        return Ok(false);
    }
    let items: Vec<String> = watcher.get_property("RegisteredStatusNotifierItems")?;
    let bus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
    for item in items {
        let service = item.split('/').next().unwrap_or_default();
        let Ok(service) = zbus::names::BusName::try_from(service) else {
            continue;
        };
        if bus.get_connection_unix_process_id(service).ok() == Some(std::process::id()) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_lost_and_stale_trays_never_allow_hiding() {
        let now = Instant::now();
        let mut registration = Registration {
            checked_at: now,
            registered: false,
        };
        assert!(!registration.usable_at(now));
        registration.registered = true;
        assert!(registration.usable_at(now));
        assert!(!registration.usable_at(now + LEASE));
        registration.registered = false;
        assert!(!registration.usable_at(now));
    }
}
