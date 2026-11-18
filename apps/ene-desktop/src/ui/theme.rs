//! Strawberry Milk Capsule palette, typography stacks and single-shadow tokens.

use crate::input::FieldLook;
use gpui::{BoxShadow, FontFallbacks, Hsla, hsla, point, px, rgb};

/// Visual token table for `ene-desktop`, verified for >= 4.5:1 text contrast.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg_surface: Hsla,
    pub bg_milk: Hsla,
    pub bg_blush: Hsla,
    pub bg_user_msg: Hsla,
    pub bg_input: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_fg: Hsla,
    pub accent_soft: Hsla,
    pub accent_soft_fg: Hsla,
    pub mint: Hsla,
    pub mint_soft: Hsla,
    pub mint_fg: Hsla,
    pub amber: Hsla,
    pub amber_soft: Hsla,
    pub amber_fg: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
    pub danger_fg: Hsla,
    pub ink: Hsla,
    pub ink_muted: Hsla,
    pub ink_placeholder: Hsla,
    pub border: Hsla,
    pub border_title: Hsla,
    pub border_float: Hsla,
    pub shadow_tint: Hsla,
}

impl Default for Theme {
    fn default() -> Self {
        Self::strawberry_milk()
    }
}

impl Theme {
    pub fn strawberry_milk() -> Self {
        Self {
            bg_surface: rgb(0xfffdfd).into(),
            bg_milk: rgb(0xfadce6).into(),
            bg_blush: rgb(0xfff4f8).into(),
            bg_user_msg: rgb(0xfce2ec).into(),
            bg_input: rgb(0xfffdfd).into(),
            accent: rgb(0xcf2e68).into(),
            accent_hover: rgb(0xb52358).into(),
            accent_fg: rgb(0xffffff).into(),
            accent_soft: rgb(0xfce4ee).into(),
            accent_soft_fg: rgb(0x7a1c40).into(),
            mint: rgb(0x229a73).into(),
            mint_soft: rgb(0xddf5ec).into(),
            mint_fg: rgb(0x16694f).into(),
            amber: rgb(0xc97a16).into(),
            amber_soft: rgb(0xfff3df).into(),
            amber_fg: rgb(0x7c4508).into(),
            danger: rgb(0xc92a52).into(),
            danger_soft: rgb(0xffe9ee).into(),
            danger_fg: rgb(0x9e1c3e).into(),
            ink: rgb(0x3b232c).into(),
            ink_muted: rgb(0x6b3b4e).into(),
            ink_placeholder: rgb(0x7d4b5e).into(),
            border: rgb(0xf3c4d5).into(),
            border_title: rgb(0xf2bdd0).into(),
            border_float: rgb(0xf09bb8).into(),
            shadow_tint: hsla(340. / 360., 0.45, 0.14, 0.16),
        }
    }

    pub fn field_look(&self, pill: bool, borderless: bool) -> FieldLook {
        FieldLook {
            bg: self.bg_input,
            border: self.border,
            border_focused: self.accent,
            text: self.ink,
            placeholder: self.ink_placeholder,
            caret: self.accent,
            selection: self.accent.alpha(0.20),
            pill,
            borderless,
        }
    }

    pub fn float_shadow(&self) -> Vec<BoxShadow> {
        vec![BoxShadow {
            color: self.shadow_tint,
            offset: point(px(0.), px(4.)),
            blur_radius: px(10.),
            spread_radius: px(0.),
        }]
    }
}

/// Primary Japanese UI font family.
pub fn ui_font_family() -> &'static str {
    "Yu Gothic UI"
}

pub fn ui_font_fallbacks() -> FontFallbacks {
    FontFallbacks::from_fonts(vec![
        "Hiragino Sans".to_owned(),
        "Meiryo".to_owned(),
        "BIZ UDPGothic".to_owned(),
        "Noto Sans CJK JP".to_owned(),
    ])
}

/// Deterministic pastel companion badge colors.
#[derive(Clone, Copy, Debug)]
pub struct CompanionPalette {
    pub bg: Hsla,
    pub fg: Hsla,
    pub ring: Hsla,
}

pub fn companion_palette(key: &str) -> CompanionPalette {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    const PALETTES: [(u32, u32, u32); 5] = [
        (0xfadce6, 0x8d2048, 0xcf2e68), // Strawberry Milk
        (0xddf5ec, 0x16694f, 0x229a73), // Mint
        (0xefe6fc, 0x4b2f8f, 0x8661d6), // Iris
        (0xffe8da, 0x883b12, 0xe07843), // Peach
        (0xdceffc, 0x18527d, 0x4698d6), // Ramune
    ];
    let (bg, fg, ring) = PALETTES[(hash % 5) as usize];
    CompanionPalette {
        bg: rgb(bg).into(),
        fg: rgb(fg).into(),
        ring: rgb(ring).into(),
    }
}
