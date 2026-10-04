//! Desktop application bindings and native lifecycle.
mod avatar_actions;
mod conversation;
mod lifecycle;
mod settings;

use crate::{
    ChatEntry, ChatWindow, SettingsWindow, Theme,
    chat::Mode,
    infrastructure::{avatar as avatar_service, tray},
};

pub(super) use avatar_actions::bind_avatar;
pub(super) use conversation::bind_chat;
pub(super) use lifecycle::bind_tray;
use lifecycle::{show_chat, show_settings};
pub(super) use settings::bind_settings;
use settings::{reflect_settings, save_settings};
use slint::{ComponentHandle, VecModel};
use std::{
    cell::RefCell,
    error::Error,
    rc::Rc,
    time::{Duration, Instant},
};

#[derive(Debug, clap::Parser)]
#[command(about = "ene 会話クライアント")]
pub(super) struct Args {
    /// 固定の仮応答を利用する（外部送信なし）
    #[arg(long)]
    demo: bool,
}
impl Args {
    pub(super) fn mode(self) -> Mode {
        if self.demo {
            Mode::Demo
        } else {
            Mode::Disconnected
        }
    }
}
