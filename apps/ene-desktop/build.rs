//! Embeds the Strawberry-Milk `ene` icon for Windows.
//!
//! GPUI Kit links GPUI's manifest with Common Controls 6 and per-monitor DPI.
//! Resource ID `1` (`ICON`) is used by the window and taskbar notification tray.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return Ok(());
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
    let sizes = [16u32, 32, 48];
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&size| (size, dib_icon_image(size)))
        .collect();

    let ico_path = out_dir.join("ene-desktop.ico");
    std::fs::write(&ico_path, build_ico(&images))?;

    let res_path = out_dir.join("ene-desktop.lib");
    std::fs::write(&res_path, build_win32_res(&images))?;

    println!("cargo:rustc-link-arg-bins={}", res_path.display());
    Ok(())
}

fn build_win32_res(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    const RT_ICON: u16 = 3;
    const RT_GROUP_ICON: u16 = 14;
    const RES_FLAGS: u16 = 0x0030;
    const LANG_EN_US: u16 = 0x0409;

    let mut res = Vec::new();
    // 32-byte Win32 .res file header record.
    append_res_record(&mut res, 0, 0, 0, 0, &[]);

    let mut group = Vec::with_capacity(6 + images.len() * 14);
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(images.len() as u16).to_le_bytes());

    for (idx, (size, data)) in images.iter().enumerate() {
        let icon_id = (idx + 1) as u16;
        append_res_record(&mut res, RT_ICON, icon_id, RES_FLAGS, LANG_EN_US, data);

        let dim = if *size >= 256 { 0 } else { *size as u8 };
        group.push(dim);
        group.push(dim);
        group.push(0);
        group.push(0);
        group.extend_from_slice(&1u16.to_le_bytes());
        group.extend_from_slice(&32u16.to_le_bytes());
        group.extend_from_slice(&(data.len() as u32).to_le_bytes());
        group.extend_from_slice(&icon_id.to_le_bytes());
    }

    append_res_record(&mut res, RT_GROUP_ICON, 1, RES_FLAGS, LANG_EN_US, &group);
    res
}

fn append_res_record(
    out: &mut Vec<u8>,
    type_id: u16,
    name_id: u16,
    flags: u16,
    lang: u16,
    data: &[u8],
) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&type_id.to_le_bytes());
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&name_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&lang.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    let pad = (4 - (data.len() % 4)) % 4;
    out.resize(out.len() + pad, 0);
}

fn build_ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    // ICONDIR header
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6u32 + 16u32 * (images.len() as u32);
    for (size, data) in images {
        let dim = *size as u8;
        out.push(dim);
        out.push(dim);
        out.push(0);
        out.push(0);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }

    for (_, data) in images {
        out.extend_from_slice(data);
    }
    out
}

fn dib_icon_image(size: u32) -> Vec<u8> {
    let and_stride = (size.div_ceil(32) * 4) as usize;
    let xor_len = (size * size * 4) as usize;
    let and_len = and_stride * (size as usize);

    let mut buf = Vec::with_capacity(40 + xor_len + and_len);
    // BITMAPINFOHEADER
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(&(size as i32).to_le_bytes());
    buf.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&32u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    buf.extend_from_slice(&((xor_len + and_len) as u32).to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&0i32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());

    let mut and_mask = vec![0u8; and_len];
    // Bottom-up BGRA rows
    for y_rev in 0..size {
        let y = size - 1 - y_rev;
        let and_row = (y_rev as usize) * and_stride;
        for x in 0..size {
            let [r, g, b, a] = render_ene_pixel(x, y, size);
            let pr = ((u16::from(r) * u16::from(a) + 127) / 255) as u8;
            let pg = ((u16::from(g) * u16::from(a) + 127) / 255) as u8;
            let pb = ((u16::from(b) * u16::from(a) + 127) / 255) as u8;
            buf.extend_from_slice(&[pb, pg, pr, a]);
            if a == 0 {
                and_mask[and_row + (x as usize) / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    buf.extend_from_slice(&and_mask);
    buf
}

/// Renders one pixel of the Strawberry-Milk capsule icon (#cf2e68 circle/squircle,
/// #f09bb8 rim, #fffdfd centered 4-point sparkle).
fn render_ene_pixel(x: u32, y: u32, size: u32) -> [u8; 4] {
    let s = size as f32;
    let cx = (s - 1.0) * 0.5;
    let cy = (s - 1.0) * 0.5;
    let dx = (x as f32 - cx) / (s * 0.5);
    let dy = (y as f32 - cy) / (s * 0.5);

    // Squircle-rounded disc SDF
    let ax = dx.abs();
    let ay = dy.abs();
    let corner = 0.34f32;
    let half = 0.90f32 - corner;
    let qx = (ax - half).max(0.0);
    let qy = (ay - half).max(0.0);
    let sdf_outer = (qx * qx + qy * qy).sqrt() - corner;

    let aa = 1.6 / s;
    let outer_alpha = smoothstep(aa, -aa, sdf_outer);
    if outer_alpha <= 0.0 {
        return [0, 0, 0, 0];
    }

    // Outer petal rim (#f09bb8) transitioning into Berry Accent (#cf2e68)
    let rim_width = (2.2 / s).max(0.11);
    let inner_mix = smoothstep(aa, -aa, sdf_outer + rim_width);
    let mut r = lerp(240.0, 207.0, inner_mix);
    let mut g = lerp(155.0, 46.0, inner_mix);
    let mut b = lerp(184.0, 104.0, inner_mix);

    // Centered 4-point sparkle (#fffdfd)
    let star_r = 0.58f32;
    let u = (ax / star_r).min(1.5);
    let v = (ay / star_r).min(1.5);
    let star_metric = u.powf(0.60) + v.powf(0.60) - 1.0;
    let star_alpha = smoothstep(aa * 1.9, -aa * 1.9, star_metric);
    r = lerp(r, 255.0, star_alpha);
    g = lerp(g, 253.0, star_alpha);
    b = lerp(b, 253.0, star_alpha);

    [
        r.round().clamp(0.0, 255.0) as u8,
        g.round().clamp(0.0, 255.0) as u8,
        b.round().clamp(0.0, 255.0) as u8,
        (outer_alpha * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
