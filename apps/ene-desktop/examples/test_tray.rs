//! Dedicated test bus only. Provides a watcher or activates a real client's menu.
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::{Arc, Mutex},
};
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
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("ENE_TRAY_TEST_PRIVATE_BUS").as_deref() != Ok("1") {
        return Err("private test D-Bus session required".into());
    }
    if let Some(action) = std::env::args().nth(1) {
        let connection = zbus::blocking::Connection::session()?;
        let watcher = zbus::blocking::Proxy::new(
            &connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )?;
        let items: Vec<String> = watcher.get_property("RegisteredStatusNotifierItems")?;
        let service = items
            .last()
            .ok_or("client tray not registered")?
            .split('/')
            .next()
            .ok_or("service missing")?
            .to_owned();
        let item = zbus::blocking::Proxy::new(
            &connection,
            service.clone(),
            "/StatusNotifierItem",
            "org.kde.StatusNotifierItem",
        )?;
        if action == "activate" {
            item.call::<_, _, ()>("Activate", &(0i32, 0i32))?;
            return Ok(());
        }
        let path: OwnedObjectPath = item.get_property("Menu")?;
        let menu =
            zbus::blocking::Proxy::new(&connection, service, path, "com.canonical.dbusmenu")?;
        type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
        let (_, (_, _, children)): (u32, Layout) =
            menu.call("GetLayout", &(0i32, 1i32, vec!["label"]))?;
        let id = children
            .into_iter()
            .find_map(|child| {
                let structure = zbus::zvariant::Structure::try_from(child).ok()?;
                let fields = structure.fields();
                let id = i32::try_from(&fields[0]).ok()?;
                let properties =
                    HashMap::<String, OwnedValue>::try_from(OwnedValue::try_from(&fields[1]).ok()?)
                        .ok()?;
                let label = <&str>::try_from(properties.get("label")?).ok()?;
                (label == action).then_some(id)
            })
            .ok_or("menu label missing")?;
        menu.call::<_, _, ()>("Event", &(id, "clicked", OwnedValue::from(0i32), 0u32))?;
    } else {
        let _connection = zbus::blocking::connection::Builder::session()?
            .name("org.kde.StatusNotifierWatcher")?
            .serve_at("/StatusNotifierWatcher", Watcher::default())?
            .build()?;
        println!("ready");
        std::io::stdout().flush()?;
        for line in std::io::stdin().lock().lines() {
            let _ = line?;
        }
    }
    Ok(())
}
