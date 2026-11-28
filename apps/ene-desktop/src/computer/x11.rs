use super::*;
use x11rb::{
    connection::Connection as _,
    protocol::{
        screensaver::{self, ConnectionExt as _},
        xproto::{ConnectionExt as _, ImageFormat, ImageOrder},
    },
    rust_connection::RustConnection,
};

pub(super) fn physical<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    f()
}

pub(super) fn desktop() -> Result<Desktop> {
    let (connection, screen) = x11rb::connect(None)?;
    let screen = &connection.setup().roots[screen];
    let idle = connection.screensaver_query_info(screen.root)?.reply()?;
    ensure!(
        idle.state == u8::from(screensaver::State::OFF),
        "screen is locked or blanked"
    );
    let focus = top_level(
        &connection,
        screen.root,
        connection.get_input_focus()?.reply()?.focus,
    )?;
    ensure!(focus != screen.root, "no focused application");
    let pid = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
    let pid = connection
        .get_property(
            false,
            focus,
            pid,
            x11rb::protocol::xproto::AtomEnum::CARDINAL,
            0,
            1,
        )?
        .reply()?;
    if let Some(pid) = pid.value32().and_then(|mut ids| ids.next()) {
        ensure!(pid != std::process::id(), "cannot automate Ene itself");
        let executable = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        ensure!(
            !executable
                .as_ref()
                .and_then(|p| p.file_name())
                .is_some_and(|name| name == "ene-avatar"),
            "cannot automate Ene itself"
        );
    }
    Ok(Desktop {
        focus: u64::from(focus),
        region: Bounds {
            x: 0,
            y: 0,
            width: u32::from(screen.width_in_pixels),
            height: u32::from(screen.height_in_pixels),
        },
        activity: Instant::now()
            .checked_sub(Duration::from_millis(u64::from(idle.ms_since_user_input))),
        tick: None,
    })
}

fn top_level(connection: &RustConnection, root: u32, mut window: u32) -> Result<u32> {
    ensure!(window > 1, "no focused application");
    for _ in 0..64 {
        let parent = connection.query_tree(window)?.reply()?.parent;
        if parent == root || window == root {
            return Ok(window);
        }
        window = parent;
    }
    anyhow::bail!("window hierarchy is unavailable")
}

pub(super) fn check_target(point: Option<(i32, i32)>, focus: u64) -> Result<()> {
    if let Some((x, y)) = point {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let target = connection
            .translate_coordinates(root, root, x.try_into()?, y.try_into()?)?
            .reply()?
            .child;
        ensure!(
            u64::from(target) == focus,
            "pointer target is outside the captured application"
        );
    }
    Ok(())
}

pub(super) fn capture(region: Bounds) -> Result<RgbImage> {
    let (connection, screen) = x11rb::connect(None)?;
    let setup = connection.setup();
    let screen = &setup.roots[screen];
    ensure!(
        u64::from(region.width) * u64::from(region.height) <= 40_000_000,
        "screen is too large"
    );
    let image = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            0,
            0,
            region.width.try_into()?,
            region.height.try_into()?,
            u32::MAX,
        )?
        .reply()?;
    let format = setup
        .pixmap_formats
        .iter()
        .find(|format| format.depth == image.depth)
        .context("X11 pixmap format missing")?;
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| &depth.visuals)
        .find(|visual| visual.visual_id == screen.root_visual)
        .context("X11 screen visual missing")?;
    let bytes = usize::from(format.bits_per_pixel / 8);
    ensure!(matches!(bytes, 3 | 4), "unsupported X11 pixel format");
    let stride = image.data.len() / region.height as usize;
    ensure!(
        stride >= region.width as usize * bytes,
        "incomplete X11 image"
    );
    let little_endian = setup.image_byte_order == ImageOrder::LSB_FIRST;
    let channel = |pixel: u32, mask: u32| {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        (u64::from((pixel & mask) >> shift) * 255 / u64::from(mask >> shift)) as u8
    };
    let mut rgb = Vec::with_capacity(region.width as usize * region.height as usize * 3);
    for row in image.data.chunks(stride).take(region.height as usize) {
        for pixel in row.chunks_exact(bytes).take(region.width as usize) {
            let mut word = [0; 4];
            let value = if little_endian {
                word[..bytes].copy_from_slice(pixel);
                u32::from_le_bytes(word)
            } else {
                word[4 - bytes..].copy_from_slice(pixel);
                u32::from_be_bytes(word)
            };
            rgb.extend([
                channel(value, visual.red_mask),
                channel(value, visual.green_mask),
                channel(value, visual.blue_mask),
            ]);
        }
    }
    RgbImage::from_raw(region.width, region.height, rgb).context("incomplete X11 image")
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::{
        Event,
        xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass},
    };

    #[tokio::test]
    #[ignore = "injects input into the test display; run under Xvfb"]
    async fn x11_captures_and_executes_all_desktop_input() -> Result<()> {
        let (connection, index) = x11rb::connect(None)?;
        let screen = &connection.setup().roots[index];
        let window = connection.generate_id()?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                screen.root,
                0,
                0,
                screen.width_in_pixels,
                screen.height_in_pixels,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new()
                    .background_pixel(screen.white_pixel)
                    .event_mask(
                        EventMask::KEY_PRESS | EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION,
                    ),
            )?
            .check()?;
        connection.map_window(window)?.check()?;
        connection
            .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)?
            .check()?;
        connection.flush()?;

        let mut frame = None;
        async fn shot(frame: &mut Option<Frame>) -> Result<Option<w::ScreenFrame>> {
            execute(frame, "test-run".into(), json!({"action":"screenshot"})).await
        }
        let image = shot(&mut frame).await?.context("empty screenshot")?;
        assert!(image.jpeg.starts_with(&[0xff, 0xd8]));
        assert_eq!(
            image::load_from_memory(&image.jpeg)?
                .to_rgb8()
                .get_pixel(0, 0)
                .0,
            [255; 3]
        );
        let incorrect = json!({"action":"key","key":"Enter","frame_id":image.id});
        assert!(
            execute(&mut frame, "wrong-run".into(), incorrect)
                .await
                .is_err()
        );
        for mut action in [
            json!({"action":"move","x":20,"y":25}),
            json!({"action":"click","x":30,"y":35,"button":"left"}),
            json!({"action":"scroll","x":30,"y":35,"steps":-1}),
            json!({"action":"type_text","text":"Hello"}),
            json!({"action":"key","key":"Enter"}),
        ] {
            let image = shot(&mut frame).await?.context("empty screenshot")?;
            action["frame_id"] = json!(image.id);
            assert!(
                execute(&mut frame, "test-run".into(), action.clone())
                    .await?
                    .is_none()
            );
            assert!(
                execute(&mut frame, "test-run".into(), action)
                    .await
                    .is_err()
            );
        }
        let stale = shot(&mut frame).await?.context("empty screenshot")?;
        connection
            .change_window_attributes(
                window,
                &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                    .background_pixel(screen.black_pixel),
            )?
            .check()?;
        connection.clear_area(false, window, 0, 0, 0, 0)?.check()?;
        assert!(
            execute(
                &mut frame,
                "test-run".into(),
                json!({"action":"key","key":"Enter","frame_id":stale.id})
            )
            .await
            .is_err()
        );
        connection.get_input_focus()?.reply()?;
        let mut motion = false;
        let mut click = false;
        let mut wheel = false;
        let mut key_events = 0;
        while let Some(event) = connection.poll_for_event()? {
            match event {
                Event::MotionNotify(_) => motion = true,
                Event::ButtonPress(event) if event.detail == 1 => click = true,
                Event::ButtonPress(event) if event.detail == 5 => wheel = true,
                Event::KeyPress(_) => key_events += 1,
                _ => {}
            }
        }
        assert!(
            motion && click && wheel && key_events >= 6,
            "missing physical events: {motion} {click} {wheel} {key_events}"
        );
        Ok(())
    }
}
