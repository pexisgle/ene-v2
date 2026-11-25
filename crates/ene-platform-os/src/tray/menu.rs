//! The tray context menu: the popup the right-click opens and the command ids
//! it reports.
//!
//! Split from the message loop so the menu's shape (which items, which order,
//! which are checked) is readable without the window procedure around it.

use super::windows_tray::{WindowContext, current_state};
use super::{
    CMD_CENTER_AVATAR, CMD_HIDE_POD, CMD_OPEN_CHAT, CMD_OPEN_MINI, CMD_QUIT,
    CMD_SETTINGS_COMPANIONS, CMD_SETTINGS_CONNECTIONS, CMD_SETTINGS_DEVICE, CMD_TOGGLE_MIC,
    TrayMode, action_for_command,
};
use crate::best_effort::ignore_teardown;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, HMENU, MENU_ITEM_FLAGS,
            MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW, SetForegroundWindow,
            SetMenuDefaultItem, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON,
            TrackPopupMenu, WM_NULL,
        },
    },
    core::PCWSTR,
    core::w,
};

pub(super) fn show_context_menu(hwnd: HWND, ctx: &WindowContext) {
    let state = current_state(ctx);
    // SAFETY: creates a new popup menu owned by this thread; the handle
    // is destroyed at the end of this function.
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };

    let chat_flags = if state.pod_open && state.pod_mode == TrayMode::ExpandedChat {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let mini_flags = if state.pod_open && state.pod_mode == TrayMode::MiniVoice {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let hide_flags = if state.pod_open {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let avatar_flags = if state.avatar_ready {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let mic_label = if state.mic_muted {
        w!("マイクのミュートを解除")
    } else {
        w!("マイクをミュート")
    };

    append_item(menu, chat_flags, CMD_OPEN_CHAT, w!("チャット画面を開く"));
    append_item(menu, mini_flags, CMD_OPEN_MINI, w!("ミニ対話ポッドを表示"));
    append_item(menu, hide_flags, CMD_HIDE_POD, w!("対話ポッドを隠す"));
    append_separator(menu);
    append_item(
        menu,
        MF_STRING,
        CMD_SETTINGS_COMPANIONS,
        w!("設定画面を開く"),
    );
    append_item(
        menu,
        MF_STRING,
        CMD_SETTINGS_CONNECTIONS,
        w!("AI・音声の接続設定"),
    );
    append_item(menu, MF_STRING, CMD_SETTINGS_DEVICE, w!("この端末の設定"));
    append_separator(menu);
    append_item(
        menu,
        avatar_flags,
        CMD_CENTER_AVATAR,
        w!("アバターを中央に戻す"),
    );
    append_item(menu, MF_STRING, CMD_TOGGLE_MIC, mic_label);
    append_separator(menu);
    append_item(menu, MF_STRING, CMD_QUIT, w!("終了"));

    if !state.pod_open {
        // SAFETY: `menu` is the live popup built above; the call only
        // marks which entry is the default.
        unsafe {
            let _discarded = SetMenuDefaultItem(menu, CMD_OPEN_CHAT, 0);
        }
    }

    let mut pt = POINT::default();
    // SAFETY: `pt` is a live local the call only writes into.
    unsafe {
        let _discarded = GetCursorPos(&mut pt);
    }
    // SAFETY: bringing our own window to the foreground is required before
    // `TrackPopupMenu`, or the menu is dismissed by the next click.
    unsafe {
        let _discarded = SetForegroundWindow(hwnd);
    }
    // SAFETY: `menu` is live and `hwnd` owns it; the call reads `pt` by
    // value and returns the chosen id without retaining anything.
    let picked = unsafe {
        TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RETURNCMD | TPM_NONOTIFY,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        )
    };
    // SAFETY: the documented workaround for the first-click bug — a
    // posted `WM_NULL` lets the menu close cleanly on our own window.
    unsafe {
        ignore_teardown(PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)));
    }
    // SAFETY: `menu` was created in this function and is not used again.
    unsafe {
        ignore_teardown(DestroyMenu(menu));
    }
    if picked.0 > 0
        && let Some(action) = action_for_command(picked.0 as u32)
    {
        let _discarded = ctx.tx.send(action);
    }
}

fn append_item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: u32, label: PCWSTR) {
    // SAFETY: `menu` is the live popup owned by the caller and `label` is a
    // `w!` literal with static lifetime.
    unsafe {
        let _discarded = AppendMenuW(menu, flags, id as usize, label);
    }
}

fn append_separator(menu: HMENU) {
    // SAFETY: same live popup; a separator takes a null label by contract.
    unsafe {
        let _discarded = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
}
