//! The SRGB render target stores gamma-encoded *linear* premultiplied colors.
//! Wayland shm expects SRGB-premultiplied ARGB: conversion matters for translucent
//! hair/materials, while opaque samples can be copied without a lookup.
use bevy::color::Srgba;
use std::sync::OnceLock;

pub fn argb(pixel: &[u8; 4]) -> [u8; 4] {
    let [r, g, b, a] = *pixel;
    match a {
        0 => [0; 4],
        255 => [b, g, r, a],
        _ => {
            static TABLE: OnceLock<Box<[u8]>> = OnceLock::new();
            let table = TABLE.get_or_init(|| {
                let mut table = vec![0; 256 * 256];
                for alpha in 1..255 {
                    for color in 0..256 {
                        let encoded = color as f32 / 255.;
                        let linear = Srgba::gamma_function(encoded);
                        let straight = (linear * 255. / alpha as f32).min(1.);
                        let srgb = Srgba::gamma_function_inverse(straight);
                        table[alpha * 256 + color] =
                            (srgb * alpha as f32).round().clamp(0., alpha as f32) as u8;
                    }
                }
                table.into_boxed_slice()
            });
            let base = a as usize * 256;
            [
                table[base + b as usize],
                table[base + g as usize],
                table[base + r as usize],
                a,
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shm_colors_preserve_alpha_without_translucent_bright_halos() {
        assert_eq!(argb(&[100, 120, 140, 255]), [140, 120, 100, 255]);
        assert_eq!(argb(&[150, 100, 50, 0]), [0; 4]);
        // Linear white at half opacity encodes to ~188 in the SRGB target,
        // but the Wayland premultiplied byte must be 128, not 188.
        assert_eq!(argb(&[188, 188, 188, 128]), [128, 128, 128, 128]);
        assert_eq!(argb(&[188, 0, 0, 128]), [0, 0, 128, 128]);
        for alpha in 1..255 {
            let result = argb(&[255, 200, 100, alpha]);
            assert_eq!(result[3], alpha);
            assert!(result[..3].iter().all(|channel| *channel <= alpha));
        }
    }
}
