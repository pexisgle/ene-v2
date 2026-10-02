mod app;
mod avatar;
mod chat;
mod file_picker;
mod host;
mod platform;
mod settings;
mod tray;

use chat::Mode;
use slint::ComponentHandle;
use std::{error::Error, rc::Rc};
slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    use clap::Parser;
    let mode = Args::parse().mode();
    let ui = AppWindow::new()?;
    let (tray, registration) = tray::Ui::new(ui.as_weak());
    let signal = ui.as_weak();
    let avatar = Rc::new(avatar::Controller::new(move || {
        let _ = signal.upgrade_in_event_loop(|ui| ui.invoke_avatar_events_available());
    }));
    let path = settings::config_path();
    let initial = settings::ClientSettings::load(&path).unwrap_or_else(|error| {
        eprintln!("{error}");
        settings::ClientSettings::default()
    });
    let app = app::Application::bind(&ui, mode, path, initial, Some(avatar.clone()), Some(tray));
    let timers = app.start_services(&ui, registration);
    file_picker::bind(&ui, &app);
    let run_result = ui.show().and_then(|_| {
        ui.invoke_focus_entry();
        slint::run_event_loop_until_quit()
    });
    avatar.shutdown();
    drop((timers, app, avatar));
    run_result?;
    Ok(())
}

#[derive(Debug, clap::Parser)]
#[command(about = "ene 会話クライアント")]
struct Args {
    /// 固定の仮応答を利用する（外部送信なし）
    #[arg(long)]
    demo: bool,
}
impl Args {
    fn mode(self) -> Mode {
        if self.demo {
            Mode::Demo
        } else {
            Mode::Disconnected
        }
    }
}

#[cfg(test)]
mod tests;
