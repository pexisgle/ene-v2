#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
use x11 as native;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as native;

use anyhow::{Context, Result, ensure};
use ene_rpc as w;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{ExtendedColorType, RgbImage, codecs::jpeg::JpegEncoder};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::time::{Duration, Instant};

const EDGE: u32 = 1280;
const MAX_JPEG: usize = 512 * 1024;
const KEYS: &[&str] = &[
    "Enter",
    "Tab",
    "Escape",
    "Backspace",
    "Delete",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Space",
];

pub(crate) fn available() -> bool {
    #[cfg(windows)]
    return true;
    #[cfg(target_os = "linux")]
    return if portal_session() {
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|v| !v.is_empty())
    } else {
        std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
    };
    #[cfg(not(any(target_os = "linux", windows)))]
    false
}

pub(crate) fn portal_session() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
}

pub(crate) fn capability() -> w::Capability {
    let point = json!({"type":"integer","minimum":0,"maximum":EDGE-1});
    let choice = |name: &str, mut fields: serde_json::Map<String, Value>| {
        fields.insert("action".into(), json!({"const":name}));
        if name != "screenshot" {
            fields.insert("frame_id".into(), json!({"type":"string","format":"uuid"}));
        }
        let required = fields.keys().cloned().collect::<Vec<_>>();
        json!({"type":"object","properties":fields,"required":required,"additionalProperties":false})
    };
    let choices = if portal_session() {
        vec![choice("screenshot", serde_json::Map::new())]
    } else {
        vec![
            choice("screenshot", serde_json::Map::new()),
            choice(
                "move",
                serde_json::Map::from_iter([
                    ("x".into(), point.clone()),
                    ("y".into(), point.clone()),
                ]),
            ),
            choice(
                "click",
                serde_json::Map::from_iter([
                    ("x".into(), point.clone()),
                    ("y".into(), point.clone()),
                    ("button".into(), json!({"enum":["left","right","double"]})),
                ]),
            ),
            choice(
                "scroll",
                serde_json::Map::from_iter([
                    ("x".into(), point.clone()),
                    ("y".into(), point),
                    (
                        "steps".into(),
                        json!({"type":"integer","minimum":-12,"maximum":12,"not":{"const":0}}),
                    ),
                ]),
            ),
            choice(
                "type_text",
                serde_json::Map::from_iter([(
                    "text".into(),
                    json!({"type":"string","minLength":1,"maxLength":512}),
                )]),
            ),
            choice(
                "key",
                serde_json::Map::from_iter([("key".into(), json!({"enum":KEYS}))]),
            ),
        ]
    };
    w::Capability {
        name: "desktop.input".into(),
        schema_json: json!({"type":"object","oneOf":choices}).to_string(),
        ..Default::default()
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Screenshot,
    Move {
        frame_id: String,
        x: u32,
        y: u32,
    },
    Click {
        frame_id: String,
        x: u32,
        y: u32,
        button: Click,
    },
    Scroll {
        frame_id: String,
        x: u32,
        y: u32,
        steps: i32,
    },
    TypeText {
        frame_id: String,
        text: String,
    },
    Key {
        frame_id: String,
        key: String,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Click {
    Left,
    Right,
    Double,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy)]
struct Desktop {
    focus: u64,
    region: Bounds,
    activity: Option<Instant>,
    tick: Option<u32>,
}

impl Desktop {
    fn matches(self, other: Self) -> bool {
        self.focus == other.focus
            && self.region == other.region
            && self.tick == other.tick
            && match (self.activity, other.activity) {
                (None, None) => true,
                (Some(old), Some(new)) => {
                    old.saturating_duration_since(new)
                        .max(new.saturating_duration_since(old))
                        <= Duration::from_millis(100)
                }
                _ => false,
            }
    }
}

pub(crate) struct Frame {
    id: String,
    run: String,
    at: Instant,
    width: u32,
    height: u32,
    desktop: Desktop,
    pixels: [u8; 32],
}

fn point(region: Bounds, size: (u32, u32), x: u32, y: u32) -> Result<(i32, i32)> {
    ensure!(x < size.0 && y < size.1, "point outside frame");
    let pixel =
        |v, dst, src| ((2 * i64::from(v) + 1) * i64::from(dst) / (2 * i64::from(src))) as i32;
    Ok((
        region.x + pixel(x, region.width, size.0),
        region.y + pixel(y, region.height, size.1),
    ))
}

pub(crate) async fn execute(
    frame: &mut Option<Frame>,
    run: String,
    args: Value,
) -> Result<Option<w::ScreenFrame>> {
    ensure!(
        available() && !run.is_empty(),
        "desktop capability unavailable"
    );
    let input = serde_json::from_value(args)?;
    #[cfg(target_os = "linux")]
    if portal_session() {
        *frame = None;
        ensure!(
            matches!(input, Input::Screenshot),
            "portal captures support screenshot only"
        );
        return Ok(Some(portal::capture().await?));
    }
    if matches!(input, Input::Screenshot) {
        *frame = None;
        let (next, wire) =
            tokio::task::spawn_blocking(move || native::physical(|| capture(run))).await??;
        *frame = Some(next);
        return Ok(Some(wire));
    }
    let frame = frame.take().context("capture a fresh screenshot first")?;
    tokio::task::spawn_blocking(move || native::physical(|| apply(frame, &run, input))).await??;
    Ok(None)
}

fn capture(run: String) -> Result<(Frame, w::ScreenFrame)> {
    let desktop = focused(None)?;
    let at = Instant::now();
    let pixels = native::capture(desktop.region)?;
    ensure!(
        desktop.matches(native::desktop()?),
        "desktop changed while capturing"
    );
    let digest = Sha256::digest(pixels.as_raw()).into();
    let wire = encode(pixels)?;
    Ok((
        Frame {
            id: wire.id.clone(),
            run,
            at,
            width: wire.width,
            height: wire.height,
            desktop,
            pixels: digest,
        },
        wire,
    ))
}

pub(crate) fn encode(pixels: RgbImage) -> Result<w::ScreenFrame> {
    let (pixels, (width, height)) = resize(pixels);
    let mut jpeg = Vec::new();
    for quality in [75, 55, 35] {
        jpeg.clear();
        JpegEncoder::new_with_quality(&mut jpeg, quality).encode(
            pixels.as_raw(),
            width,
            height,
            ExtendedColorType::Rgb8,
        )?;
        if jpeg.len() <= MAX_JPEG {
            break;
        }
    }
    ensure!(
        jpeg.len() <= MAX_JPEG,
        "screen capture exceeds the supported size"
    );
    Ok(w::ScreenFrame {
        id: crate::network::id(),
        width,
        height,
        jpeg,
        ..Default::default()
    })
}

fn resize(pixels: RgbImage) -> (RgbImage, (u32, u32)) {
    let size = pixels.dimensions();
    let edge = size.0.max(size.1);
    if edge <= EDGE {
        return (pixels, size);
    }
    let scale = |v| (u64::from(v) * u64::from(EDGE) / u64::from(edge)).max(1) as u32;
    let size = (scale(size.0), scale(size.1));
    (
        image::imageops::resize(
            &pixels,
            size.0,
            size.1,
            image::imageops::FilterType::Triangle,
        ),
        size,
    )
}

fn focused(expected: Option<Desktop>) -> Result<Desktop> {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Ok(desktop) = native::desktop()
            && expected.is_none_or(|before| {
                before.focus == desktop.focus && before.region == desktop.region
            })
        {
            return Ok(desktop);
        }
        ensure!(
            Instant::now() < until,
            "the captured application is not focused"
        );
        std::thread::sleep(Duration::from_millis(125));
    }
}

fn apply(frame: Frame, run: &str, input: Input) -> Result<()> {
    let expected = match &input {
        Input::Move { frame_id, .. }
        | Input::Click { frame_id, .. }
        | Input::Scroll { frame_id, .. }
        | Input::TypeText { frame_id, .. }
        | Input::Key { frame_id, .. } => frame_id,
        Input::Screenshot => anyhow::bail!("screenshot is not an input action"),
    };
    ensure!(
        frame.id == *expected && frame.run == run && frame.at.elapsed() < Duration::from_secs(60),
        "capture a fresh screenshot for this run"
    );
    let coordinates = match &input {
        Input::Move { x, y, .. } | Input::Click { x, y, .. } | Input::Scroll { x, y, .. } => Some(
            point(frame.desktop.region, (frame.width, frame.height), *x, *y)?,
        ),
        _ => None,
    };
    let current = focused(Some(frame.desktop))?;
    let pixels = native::capture(current.region)?;
    ensure!(
        <[u8; 32]>::from(Sha256::digest(pixels.as_raw())) == frame.pixels,
        "the screen changed since capture"
    );
    let mut enigo = Enigo::new(&Settings::default())?;
    ensure!(
        current.matches(native::desktop()?),
        "desktop changed during verification"
    );
    native::check_target(coordinates, frame.desktop.focus)?;
    if let Some((x, y)) = coordinates {
        enigo.move_mouse(x, y, Coordinate::Abs)?;
    }
    match input {
        Input::Move { .. } => {}
        Input::Click { button, .. } => {
            enigo.button(
                if matches!(button, Click::Right) {
                    Button::Right
                } else {
                    Button::Left
                },
                Direction::Click,
            )?;
            if matches!(button, Click::Double) {
                enigo.button(Button::Left, Direction::Click)?;
            }
        }
        Input::Scroll { steps, .. } => {
            ensure!(
                steps != 0 && (-12..=12).contains(&steps),
                "invalid scroll distance"
            );
            enigo.scroll(-steps, Axis::Vertical)?;
        }
        Input::TypeText { text, .. } => {
            ensure!(
                !text.is_empty()
                    && text.encode_utf16().count() <= 512
                    && !text.chars().any(char::is_control),
                "enter at most 512 units of plain text"
            );
            enigo.text(&text)?;
        }
        Input::Key { key, .. } => enigo.key(
            match key.as_str() {
                "Enter" => Key::Return,
                "Tab" => Key::Tab,
                "Escape" => Key::Escape,
                "Backspace" => Key::Backspace,
                "Delete" => Key::Delete,
                "Home" => Key::Home,
                "End" => Key::End,
                "PageUp" => Key::PageUp,
                "PageDown" => Key::PageDown,
                "ArrowUp" => Key::UpArrow,
                "ArrowDown" => Key::DownArrow,
                "ArrowLeft" => Key::LeftArrow,
                "ArrowRight" => Key::RightArrow,
                "Space" => Key::Space,
                _ => anyhow::bail!("unsupported key"),
            },
            Direction::Click,
        )?,
        Input::Screenshot => unreachable!(),
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
mod native {
    use super::*;

    pub(super) fn physical<T>(_: impl FnOnce() -> Result<T>) -> Result<T> {
        anyhow::bail!("desktop capability unavailable")
    }

    pub(super) fn desktop() -> Result<Desktop> {
        anyhow::bail!("desktop capability unavailable")
    }

    pub(super) fn capture(_: Bounds) -> Result<RgbImage> {
        anyhow::bail!("desktop capability unavailable")
    }

    pub(super) fn check_target(_: Option<(i32, i32)>, _: u64) -> Result<()> {
        anyhow::bail!("desktop capability unavailable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_coordinates_preserve_negative_origins() -> Result<()> {
        let rect = Bounds {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(point(rect, (1280, 720), 0, 0)?, (-1920, 0));
        assert_eq!(point(rect, (1280, 720), 1279, 719)?, (-1, 1079));
        assert!(point(rect, (1280, 720), 1280, 719).is_err());
        Ok(())
    }
}
