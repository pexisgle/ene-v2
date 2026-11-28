use super::*;
pub(super) use ene_platform_os::windows::{check_target, physical};

pub(super) fn desktop() -> Result<Desktop> {
    let native = ene_platform_os::windows::foreground_monitor()?;
    Ok(Desktop {
        focus: native.focus,
        region: Bounds {
            x: native.x,
            y: native.y,
            width: native.width,
            height: native.height,
        },
        activity: None,
        tick: Some(native.input_tick),
    })
}

pub(super) fn capture(region: Bounds) -> Result<RgbImage> {
    let monitor = xcap::Monitor::from_point(region.x, region.y)?;
    let pixels = monitor.capture_image()?;
    ensure!(
        pixels.dimensions() == (region.width, region.height),
        "foreground monitor changed"
    );
    Ok(image::DynamicImage::ImageRgba8(pixels).into_rgb8())
}
