//! Virtual pointer for an isolated headless compositor's integration tests.
use std::io::{BufRead, Write};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_pointer, wl_registry, wl_seat},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
struct State;
impl Dispatch<wl_registry::WlRegistry, wayland_client::globals::GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &wayland_client::globals::GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(State: ignore ZwlrVirtualPointerV1);
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("ENE_AVATAR_TEST_PRIVATE_COMPOSITOR").as_deref() != Ok("1") {
        return Err("isolated test compositor required".into());
    }
    let connection = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<State>(&connection)?;
    let qh = queue.handle();
    let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ())?;
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ())?;
    let pointer = manager.create_virtual_pointer(Some(&seat), &qh, ());
    queue.roundtrip(&mut State)?;
    println!("ready");
    std::io::stdout().flush()?;
    for line in std::io::stdin().lock().lines() {
        let command: serde_json::Value = serde_json::from_str(&line?)?;
        match command["type"].as_str() {
            Some("move") => pointer.motion(
                0,
                command["x"].as_f64().ok_or("x")?,
                command["y"].as_f64().ok_or("y")?,
            ),
            Some("set") => pointer.motion_absolute(
                0,
                command["x"].as_u64().ok_or("x")? as u32,
                command["y"].as_u64().ok_or("y")? as u32,
                1280,
                900,
            ),
            Some("press") => pointer.button(0, 0x110, wl_pointer::ButtonState::Pressed),
            Some("release") => pointer.button(0, 0x110, wl_pointer::ButtonState::Released),
            _ => return Err("unknown pointer action".into()),
        }
        pointer.frame();
        queue.roundtrip(&mut State)?;
        println!("done");
        std::io::stdout().flush()?;
    }
    pointer.destroy();
    connection.flush()?;
    Ok(())
}
