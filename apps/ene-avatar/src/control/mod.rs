//! JSON Lines control channel and application lifecycle.

mod output;
mod protocol;
mod session;

#[cfg(test)]
mod tests;

use bevy::{
    app::{AppExit, MainScheduleOrder},
    ecs::schedule::ScheduleLabel,
    prelude::*,
};
use protocol::{Command, Request};
use serde_json::Value;
use std::{
    io::{BufRead, Read},
    sync::mpsc::{self, Receiver, SyncSender},
    time::Duration,
};

#[derive(Resource)]
pub struct Control {
    pub output: SyncSender<Value>,
    pub fps: u32,
    #[cfg(target_os = "linux")]
    pub wake_tx: exwlshellev::calloop::channel::Sender<()>,
}

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameEnd;

pub fn run(mut app: App) -> AppExit {
    let done = app
        .world_mut()
        .remove_non_send::<Receiver<std::io::Result<()>>>()
        .expect("control plugin must be installed");
    let exit = app.run();
    match done.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => exit,
        result => {
            error!("renderer output did not finish: {result:?}");
            AppExit::error()
        }
    }
}

pub fn setup(app: &mut App) {
    let (input_tx, input) = mpsc::sync_channel(64);
    let (output, output_rx) = mpsc::sync_channel::<Value>(64);
    let (done, output_done) = mpsc::channel();
    #[cfg(target_os = "linux")]
    let (wake_tx, wake_rx) = exwlshellev::calloop::channel::channel();
    #[cfg(target_os = "linux")]
    app.insert_non_send(wake_rx);
    #[cfg(target_os = "linux")]
    let input_wake = wake_tx.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let mut line = String::new();
            let result = (&mut stdin).take(65_537).read_line(&mut line);
            let request = match result {
                Ok(0) => Ok(Request {
                    id: 0,
                    command: Command::Quit,
                }),
                Ok(n) if n <= 65_536 => serde_json::from_str(&line).map_err(|e| e.to_string()),
                _ => Err("input line exceeds 64 KiB or could not be read".into()),
            };
            let end = line.len() > 65_536 || line.is_empty();
            if input_tx.send(request).is_err() {
                break;
            }
            #[cfg(target_os = "linux")]
            let _ = input_wake.send(());
            if end {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let result = output::write_output(std::io::stdout().lock(), output_rx);
        let _ = done.send(result);
    });
    app.insert_non_send(output_done)
        .insert_non_send(input)
        .insert_resource(Control {
            output,
            fps: 30,
            #[cfg(target_os = "linux")]
            wake_tx,
        })
        .add_systems(PreUpdate, session::requests)
        .add_systems(
            PostUpdate,
            output::publish.after(crate::character::notify_finished),
        );
    app.world_mut()
        .resource_mut::<MainScheduleOrder>()
        .insert_after(Last, FrameEnd);
}
