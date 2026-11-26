//! Native tray context menu and its application actions.

use super::{TrayAction, TrayMode, TraySection, TrayState, windows_tray::TrayError};
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};

pub(super) fn build(state: &TrayState) -> Result<Menu, TrayError> {
    Ok(Menu::with_items(&[
        &CheckMenuItem::with_id(
            "chat",
            "チャット画面を開く",
            true,
            state.pod_open && state.pod_mode == TrayMode::ExpandedChat,
            None,
        ),
        &CheckMenuItem::with_id(
            "mini",
            "ミニ対話ポッドを表示",
            true,
            state.pod_open && state.pod_mode == TrayMode::MiniVoice,
            None,
        ),
        &MenuItem::with_id("hide", "対話ポッドを隠す", state.pod_open, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("settings", "設定画面を開く", true, None),
        &MenuItem::with_id("connections", "AI・音声の接続設定", true, None),
        &MenuItem::with_id("device", "この端末の設定", true, None),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("avatar", "アバターを中央に戻す", state.avatar_ready, None),
        &MenuItem::with_id(
            "mic",
            if state.mic_muted {
                "マイクのミュートを解除"
            } else {
                "マイクをミュート"
            },
            true,
            None,
        ),
        &PredefinedMenuItem::separator(),
        &MenuItem::with_id("quit", "終了", true, None),
    ])?)
}

pub(super) fn action(id: &str) -> Option<TrayAction> {
    Some(match id {
        "chat" => TrayAction::OpenChat,
        "mini" => TrayAction::OpenMiniPod,
        "hide" => TrayAction::HidePod,
        "settings" => TrayAction::OpenSettings(TraySection::Companions),
        "connections" => TrayAction::OpenSettings(TraySection::Connections),
        "device" => TrayAction::OpenSettings(TraySection::Device),
        "avatar" => TrayAction::CenterAvatar,
        "mic" => TrayAction::ToggleMicMute,
        "quit" => TrayAction::Quit,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_map_to_application_actions() {
        assert_eq!(action("chat"), Some(TrayAction::OpenChat));
        assert_eq!(action("mini"), Some(TrayAction::OpenMiniPod));
        assert_eq!(action("hide"), Some(TrayAction::HidePod));
        assert_eq!(
            action("settings"),
            Some(TrayAction::OpenSettings(TraySection::Companions))
        );
        assert_eq!(
            action("connections"),
            Some(TrayAction::OpenSettings(TraySection::Connections))
        );
        assert_eq!(
            action("device"),
            Some(TrayAction::OpenSettings(TraySection::Device))
        );
        assert_eq!(action("avatar"), Some(TrayAction::CenterAvatar));
        assert_eq!(action("mic"), Some(TrayAction::ToggleMicMute));
        assert_eq!(action("quit"), Some(TrayAction::Quit));
        assert_eq!(action("unknown"), None);
    }
}
