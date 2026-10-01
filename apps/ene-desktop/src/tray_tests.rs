//! Native tray integration against an isolated StatusNotifierWatcher, using the
//! actual Slint/ksni service and actual D-Bus activation/menu calls.
use super::*;
use slint::platform::WindowEvent;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

#[derive(Clone, Default)]
struct Watcher {
    items: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    fn register_status_notifier_item(&self, service: String) {
        self.items
            .lock()
            .unwrap()
            .push(format!("{service}/StatusNotifierItem"));
    }

    fn register_status_notifier_host(&self, _service: String) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.lock().unwrap().clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

fn send_action(
    items: &Arc<Mutex<Vec<String>>>,
    action: &'static str,
    result: mpsc::Sender<zbus::Result<()>>,
) {
    let service = items.lock().unwrap()[0]
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    std::thread::spawn(move || {
        let operation = || -> zbus::Result<()> {
            let connection = zbus::blocking::Connection::session()?;
            let item = zbus::blocking::Proxy::new(
                &connection,
                service.clone(),
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
            )?;
            if action == "activate" {
                return item.call("Activate", &(0i32, 0i32));
            }
            let path: OwnedObjectPath = item.get_property("Menu")?;
            let menu = zbus::blocking::Proxy::new(
                &connection,
                service.clone(),
                path,
                "com.canonical.dbusmenu",
            )?;
            // Read IDs from GetLayout, as real tray hosts do. ksni 0.3.6's
            // GetGroupProperties([], ...) returns raw indices without its
            // layout revision offset, so those values are not Event IDs.
            type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
            let (_, (_, _, children)): (u32, Layout) =
                menu.call("GetLayout", &(0i32, 1i32, vec!["label"]))?;
            let id = children
                .into_iter()
                .find_map(|child| {
                    let structure = zbus::zvariant::Structure::try_from(child).ok()?;
                    let fields = structure.fields();
                    let id = i32::try_from(&fields[0]).ok()?;
                    let properties = HashMap::<String, OwnedValue>::try_from(
                        OwnedValue::try_from(&fields[1]).ok()?,
                    )
                    .ok()?;
                    let label = properties.get("label")?;
                    let label = <&str>::try_from(label).ok()?;
                    (label == action).then_some(id)
                })
                .expect("Slint tray menu label missing");
            eprintln!("D-Bus menu activation: {action}, id={id}");
            menu.call("Event", &(id, "clicked", OwnedValue::from(0i32), 0u32))
        };
        // Quit can drop the receiver before the D-Bus reply arrives.
        let _ = result.send(operation());
    });
}

#[test]
#[ignore = "requires an isolated D-Bus session, Wayland and a rendering adapter; use just test-desktop-tray"]
fn wayland_tray_lifecycle() {
    assert_eq!(
        std::env::var("ENE_TRAY_TEST_PRIVATE_BUS").as_deref(),
        Ok("1")
    );
    let watcher = Watcher::default();
    let items = watcher.items.clone();
    let connection = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at("/StatusNotifierWatcher", watcher)
        .unwrap()
        .build()
        .unwrap();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("femtovg-wgpu".into())
        .select()
        .unwrap();
    let ui = ChatWindow::new().unwrap();
    let settings = SettingsWindow::new().unwrap();
    bind_chat(&ui, Mode::Demo);
    bind_settings(&ui, &settings);
    let (_tray, _monitor_timer) = bind_tray(&ui, &settings).unwrap();
    ui.show().unwrap();
    let (sender, receiver) = mpsc::channel::<zbus::Result<()>>();
    let weak = ui.as_weak();
    let mut step = 0;
    let done = Rc::new(std::cell::Cell::new(false));
    let completed = done.clone();
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "tray lifecycle timed out at step {step}"
            );
            for reply in receiver.try_iter() {
                reply.unwrap();
            }
            let ui = weak.upgrade().unwrap();
            match step {
                0 if ui.get_tray_ready() => {
                    // Closing the last visible window keeps the event loop alive.
                    ui.window().dispatch_event(WindowEvent::CloseRequested);
                    assert!(!ui.window().is_visible());
                    send_action(&items, "activate", sender.clone());
                    step = 1;
                }
                1 if ui.window().is_visible() => {
                    assert!(ui.get_editor_focused());
                    send_action(&items, "設定", sender.clone());
                    step = 2;
                }
                2 if settings.window().is_visible() => {
                    settings.invoke_apply_settings();
                    assert!(!settings.window().is_visible());
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                    assert!(!ui.window().is_visible());
                    send_action(&items, "会話を表示", sender.clone());
                    step = 3;
                }
                3 if ui.window().is_visible() => {
                    ui.window()
                        .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                    assert!(!ui.window().is_visible());
                    // Lose the watcher while hidden, then verify automatic rescue.
                    let connection = connection.clone();
                    std::thread::spawn(move || {
                        connection
                            .release_name("org.kde.StatusNotifierWatcher")
                            .unwrap();
                    });
                    step = 4;
                }
                4 if ui.window().is_visible() && !ui.get_tray_ready() => {
                    ui.window().dispatch_event(WindowEvent::CloseRequested);
                    assert!(ui.window().is_visible());
                    // The SNI service remains addressable after watcher loss. This
                    // invokes the actual menu callback and verifies explicit quit.
                    completed.set(true);
                    send_action(&items, "クライアント終了", sender.clone());
                    step = 5;
                }
                _ => {}
            }
        },
    );
    slint::run_event_loop_until_quit().unwrap();
    assert!(
        done.get(),
        "event loop exited before lifecycle checks completed"
    );
}
