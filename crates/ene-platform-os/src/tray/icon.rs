//! The tray icon bitmap: the executable's embedded icon when it has one, and a
//! drawn fallback when it does not.
//!
//! Split from the message loop because an icon is a GDI resource with its own
//! lifetime rules, and none of that belongs in the window procedure.

use std::ffi::c_void;
use windows::{
    Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
            DeleteObject,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, IMAGE_FLAGS, IMAGE_ICON,
            LoadIconW, LoadImageW, SM_CXSMICON, SM_CYSMICON, USER_DEFAULT_SCREEN_DPI,
        },
    },
    core::PCWSTR,
};

pub(super) fn load_or_create_tray_icon() -> (HICON, bool) {
    // SAFETY: reads a metric index; takes no pointer and cannot fail.
    let cx = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    // SAFETY: as above, for the vertical metric.
    let cy = unsafe { GetSystemMetrics(SM_CYSMICON) }.max(16);
    // SAFETY: `None` names this process's own module, alive throughout.
    if let Ok(module) = unsafe { GetModuleHandleW(None) } {
        // SAFETY: the module handle is live; the resource id is the
        // process's own embedded icon and the sizes came from the metrics
        // above. The returned handle is checked before use.
        if let Ok(handle) = unsafe {
            LoadImageW(
                Some(module.into()),
                PCWSTR(std::ptr::without_provenance(1)),
                IMAGE_ICON,
                cx,
                cy,
                IMAGE_FLAGS(0),
            )
        } && !handle.is_invalid()
        {
            return (HICON(handle.0), true);
        }
        // SAFETY: same live module and its own embedded icon resource; the
        // returned handle is checked for validity before it is used.
        let icon =
            unsafe { LoadIconW(Some(module.into()), PCWSTR(std::ptr::without_provenance(1))) };
        if let Ok(icon) = icon
            && !icon.is_invalid()
        {
            return (icon, false);
        }
    }
    (
        create_fallback_icon(cx.min(64) as u32).unwrap_or_default(),
        true,
    )
}

fn create_fallback_icon(size: u32) -> Option<HICON> {
    let size = size.max(16);
    let stride = (size * 4) as usize;
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size as i32,
            biHeight: -(size as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bmi` describes a top-down 32bpp DIB and `bits` is a live
    // local the call points at the new pixel buffer through.
    let color = unsafe { CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }.ok()?;
    if bits.is_null() {
        // SAFETY: the section was created but produced no pixels, so it
        // is released here instead of being handed out.
        unsafe {
            let _discarded = DeleteObject(color.into());
        }
        return None;
    }
    // SAFETY: `bits` points at `stride * size` bytes of a top-down
    // 32bpp DIB created above, which the loop below writes within.
    let pixels =
        unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), stride * (size as usize)) };
    let s = size as f32;
    let c = (s - 1.0) * 0.5;
    let r = s * 0.45;
    let _discarded = USER_DEFAULT_SCREEN_DPI;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - c) / r;
            let dy = (y as f32 - c) / r;
            let dist = (dx * dx + dy * dy).sqrt();
            let off = ((y * size + x) * 4) as usize;
            if dist <= 1.0 {
                let star = dx.abs().sqrt() + dy.abs().sqrt() <= 0.85;
                let (pr, pg, pb) = if star {
                    (255, 253, 253)
                } else {
                    (207, 46, 104)
                };
                pixels[off] = pb;
                pixels[off + 1] = pg;
                pixels[off + 2] = pr;
                pixels[off + 3] = 255;
            } else {
                pixels[off..off + 4].fill(0);
            }
        }
    }
    let mask_stride = (size.div_ceil(16) * 2) as usize;
    let mask_bytes = vec![0u8; mask_stride * (size as usize)];
    // SAFETY: `mask_bytes` is a live 1bpp buffer of exactly the size the
    // dimensions describe; `CreateBitmap` copies it.
    let mask = unsafe {
        CreateBitmap(
            size as i32,
            size as i32,
            1,
            1,
            Some(mask_bytes.as_ptr().cast()),
        )
    };
    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: `info` names the two bitmaps just created; the call reads
    // them and returns an independent icon handle.
    let icon = unsafe { CreateIconIndirect(&info) }.ok();
    // SAFETY: `CreateIconIndirect` copied the bitmaps, so both handles
    // are ours to release; the mask is skipped when creation failed.
    unsafe {
        if !mask.is_invalid() {
            let _discarded = DeleteObject(mask.into());
        }
    }
    // SAFETY: the colour bitmap is always created before this point.
    unsafe {
        let _discarded = DeleteObject(color.into());
    }
    icon
}
