mod application;
mod host_model;
mod infrastructure;
mod presentation;
mod settings;

use application::demo::Mode;
use infrastructure::{avatar, file_picker, tray};
use slint::ComponentHandle;
use std::{error::Error, rc::Rc, sync::Arc};
slint::include_modules!();

fn main() -> Result<(), Box<dyn Error>> {
    use clap::Parser;
    let mode = Args::parse().mode();
    let ui = AppWindow::new()?;
    let (tray, registration) = tray::Ui::new(ui.as_weak());
    let signal = ui.as_weak();
    let avatar = Rc::new(avatar::Controller::new(Arc::new(move || {
        let _ = signal.upgrade_in_event_loop(|ui| ui.invoke_avatar_events_available());
    })));
    let path = settings::config_path();
    let (initial, load_error) = match settings::ClientSettings::load(&path) {
        Ok(initial) => (initial, None),
        Err(error) => (settings::ClientSettings::default(), Some(error)),
    };
    let app =
        application::Application::bind(&ui, mode, path, initial, Some(avatar.clone()), Some(tray));
    if let Some(error) = load_error {
        ui.global::<UiState>().set_save_failed(true);
        ui.global::<UiState>()
            .set_save_status(format!("{error}。元の設定は次の保存時に退避します。").into());
    }
    let timers = app.start_services(&ui, registration);
    file_picker::bind(&ui, &app);
    let run_result = ui.show().and_then(|_| {
        ui.invoke_focus_entry();
        slint::run_event_loop_until_quit()
    });
    app.shutdown(&ui);
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
