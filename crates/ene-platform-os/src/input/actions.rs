//! Maps desktop input actions to a single Windows input batch.
use super::{Desktop, UnsafeToSend, check_before_send, is_release, send_input};
use serde::Deserialize;
use windows::Win32::UI::Input::KeyboardAndMouse as input;

/// A desktop point in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Double,
}

macro_rules! keys {
    ($($key:ident => $vk:ident),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Deserialize)]
        pub enum Key { $($key),+ }

        impl Key {
            pub const NAMES: &[&str] = &[$(stringify!($key)),+];

            fn virtual_key(self) -> input::VIRTUAL_KEY {
                match self { $(Self::$key => input::$vk),+ }
            }
        }
    };
}

keys! {
    Enter => VK_RETURN,
    Tab => VK_TAB,
    Escape => VK_ESCAPE,
    Backspace => VK_BACK,
    Delete => VK_DELETE,
    Home => VK_HOME,
    End => VK_END,
    PageUp => VK_PRIOR,
    PageDown => VK_NEXT,
    ArrowUp => VK_UP,
    ArrowDown => VK_DOWN,
    ArrowLeft => VK_LEFT,
    ArrowRight => VK_RIGHT,
    Space => VK_SPACE,
}

/// Input to apply after observing the current desktop.
#[derive(Clone, Copy, Debug)]
pub enum Input<'a> {
    Click { point: Point, button: Button },
    Scroll { point: Point, steps: i32 },
    Text(&'a str),
    Key(Key),
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error(transparent)]
    Refused(#[from] UnsafeToSend),
    #[error("the pointer position cannot be addressed on the current desktop")]
    PointerUnavailable,
    #[error("Windows rejected the input")]
    Rejected,
    #[error("Windows accepted only part of the input; outcome unknown, do not retry it")]
    Partial,
}

/// Applies a batch after checking that the captured desktop and target are still current.
pub fn apply_input(
    desktop: &Desktop,
    action: Input<'_>,
    avatar_pid: Option<u32>,
) -> Result<(), InputError> {
    let mut events = Vec::new();
    let check_focus = !matches!(action, Input::Click { .. });
    let target = match action {
        Input::Click { point, button } => {
            let (down, up) = match button {
                Button::Left | Button::Double => {
                    (input::MOUSEEVENTF_LEFTDOWN, input::MOUSEEVENTF_LEFTUP)
                }
                Button::Right => (input::MOUSEEVENTF_RIGHTDOWN, input::MOUSEEVENTF_RIGHTUP),
            };
            events.push(movement(desktop, point)?);
            events.extend([mouse(down, 0, 0, 0), mouse(up, 0, 0, 0)]);
            if matches!(button, Button::Double) {
                events.extend([mouse(down, 0, 0, 0), mouse(up, 0, 0, 0)]);
            }
            Some(point)
        }
        Input::Scroll { point, steps } => {
            events.push(movement(desktop, point)?);
            events.push(mouse(
                input::MOUSEEVENTF_WHEEL,
                0,
                0,
                steps.saturating_mul(120) as u32,
            ));
            Some(point)
        }
        Input::Text(text) => {
            for code in text.encode_utf16() {
                events.extend(pair(input::VIRTUAL_KEY(0), code, input::KEYEVENTF_UNICODE));
            }
            None
        }
        Input::Key(key) => {
            let key = key.virtual_key();
            let flags = match key {
                input::VK_LEFT
                | input::VK_RIGHT
                | input::VK_UP
                | input::VK_DOWN
                | input::VK_DELETE
                | input::VK_HOME
                | input::VK_END
                | input::VK_PRIOR
                | input::VK_NEXT => input::KEYEVENTF_EXTENDEDKEY,
                _ => input::KEYBD_EVENT_FLAGS(0),
            };
            events.extend(pair(key, 0, flags));
            None
        }
    };
    check_before_send(desktop, target, check_focus, avatar_pid)?;
    let sent = send_input(&events);
    if sent == events.len() {
        return Ok(());
    }
    if sent == 0 {
        return Err(InputError::Rejected);
    }
    // Each press is immediately paired with its release.
    if let Some(release) = events.get(sent).filter(|event| is_release(event)) {
        send_input(std::slice::from_ref(release));
    }
    Err(InputError::Partial)
}

fn movement(desktop: &Desktop, point: Point) -> Result<input::INPUT, InputError> {
    let screen = desktop.virtual_screen();
    let axis = |position, start, end| -> Result<i32, InputError> {
        if !(start..end).contains(&position) || end - start > 65536 {
            return Err(InputError::PointerUnavailable);
        }
        Ok(((2 * i64::from(position - start) + 1) * 65536 / (2 * i64::from(end - start))) as i32)
    };
    Ok(mouse(
        input::MOUSEEVENTF_MOVE | input::MOUSEEVENTF_ABSOLUTE | input::MOUSEEVENTF_VIRTUALDESK,
        axis(point.x, screen.left, screen.right)?,
        axis(point.y, screen.top, screen.bottom)?,
        0,
    ))
}

fn mouse(flags: input::MOUSE_EVENT_FLAGS, x: i32, y: i32, data: u32) -> input::INPUT {
    input::INPUT {
        r#type: input::INPUT_MOUSE,
        Anonymous: input::INPUT_0 {
            mi: input::MOUSEINPUT {
                dx: x,
                dy: y,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn pair(key: input::VIRTUAL_KEY, scan: u16, flags: input::KEYBD_EVENT_FLAGS) -> [input::INPUT; 2] {
    [flags, flags | input::KEYEVENTF_KEYUP].map(|dw_flags| input::INPUT {
        r#type: input::INPUT_KEYBOARD,
        Anonymous: input::INPUT_0 {
            ki: input::KEYBDINPUT {
                wVk: key,
                wScan: scan,
                dwFlags: dw_flags,
                ..Default::default()
            },
        },
    })
}
