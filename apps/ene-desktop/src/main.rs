mod application;
mod chat;
mod infrastructure;

use application::{Args, bind_avatar, bind_chat, bind_settings, bind_tray};
use infrastructure::file_picker;
use slint::ComponentHandle;
use std::error::Error;
slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    use clap::Parser;
    let mode = Args::parse().mode();
    let ui = ChatWindow::new()?;
    let settings = SettingsWindow::new()?;
    bind_chat(&ui, mode);
    bind_settings(&ui, &settings);
    file_picker::bind(&settings);

    let (tray, monitor_timer) = bind_tray(&ui, &settings)?;
    let (avatar, avatar_timer) = bind_avatar(&ui, &settings, &tray);
    let run_result = ui.show().and_then(|_| {
        ui.invoke_focus_input();
        // Hiding the last window must not stop the client event loop.
        slint::run_event_loop_until_quit()
    });
    // Slint can retain timer callback captures after its loop stops. Explicitly
    // join the supervisor while the UI is still alive, rather than relying on
    // the last Rc being dropped during process teardown.
    avatar.shutdown();
    drop((avatar_timer, avatar, monitor_timer, tray, settings));
    run_result?;
    Ok(())
}
