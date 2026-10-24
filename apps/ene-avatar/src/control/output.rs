//! Serializing replies and notifications to standard output.

use super::Control;
use crate::character::Notification;
use bevy::{app::AppExit, prelude::*};
use serde_json::Value;
use std::io::Write;
use std::sync::mpsc::Receiver;

pub(super) fn write_output(mut writer: impl Write, output: Receiver<Value>) -> std::io::Result<()> {
    for message in output {
        writeln!(writer, "{message}")?;
        writer.flush()?;
    }
    Ok(())
}

/// Flush notifications after command replies, transferring ownership to stdout.
pub(super) fn publish(
    control: Res<Control>,
    mut events: ResMut<Messages<Notification>>,
    mut exit: MessageWriter<AppExit>,
) {
    for Notification(event) in events.drain() {
        if let Err(error) = control.output.try_send(event) {
            error!("renderer output unavailable: {error}");
            exit.write(AppExit::error());
            break;
        }
    }
}
