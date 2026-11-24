//! Strawberry Milk Capsule palette, typography stacks and single-shadow tokens.

use crate::input::FieldLook;
use gpui::{BoxShadow, FontFallbacks, Hsla, hsla, point, px, rgb};

/// Corner radii, named by how large the surface is.
///
/// These are the values the UI already used as bare literals; naming them makes
/// a mismatch visible instead of looking like a typo.
pub mod radius {
    use gpui::{Pixels, px};

    /// Tags, pills and small chips.
    pub fn small() -> Pixels {
        px(8.)
    }
    /// Inputs and inline cards.
    pub fn medium() -> Pixels {
        px(10.)
    }
    /// Menus and grouped rows.
    pub fn large() -> Pixels {
        px(12.)
    }
    /// The pod and the settings panes.
    pub fn surface() -> Pixels {
        px(14.)
    }
    /// The floating orb.
    pub fn orb() -> Pixels {
        px(18.)
    }
}

/// Visual token table for `ene-desktop`.
///
/// [`Theme::strawberry_milk`] uses pastel tints on a near-white surface and
/// [`Theme::midnight_berry`] the same roles on a near-black one.
/// `midnight_berry_text_reaches_aa_contrast` asserts the dark palette's text
/// pairs reach 4.5:1; the light palette's ratios were verified by hand.
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// The light palette, so an unwired caller keeps the pre-dark rendering.
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

    /// Dark counterpart to [`Theme::strawberry_milk`], same roles and hues.
    ///
    /// The surfaces keep the strawberry hue as a low-saturation near-black
    /// plum instead of neutral grey, so the pod still reads as the same
    /// product. `accent` is `#bb2c5e`, the `--berry` hue from the website
    /// (`#a82554`) lifted to 45% lightness: at the website's 40% it only
    /// reaches 2.70:1 against `bg_surface`, and `accent` doubles as an icon
    /// tint and focus border there, which needs 3:1.
    // Remove this allow once `Hub::theme` or a settings toggle selects the palette.
    #[allow(dead_code)]
    pub fn midnight_berry() -> Self {
        Self {
            bg_surface: rgb(0x1a1114).into(),
            bg_milk: rgb(0x2a1620).into(),
            bg_blush: rgb(0x221419).into(),
            bg_user_msg: rgb(0x2d1822).into(),
            bg_input: rgb(0x241519).into(),
            accent: rgb(0xbb2c5e).into(),
            accent_hover: rgb(0xcc3a6b).into(),
            accent_fg: rgb(0xffffff).into(),
            accent_soft: rgb(0x38202c).into(),
            accent_soft_fg: rgb(0xf0a3bf).into(),
            mint: rgb(0x34c495).into(),
            mint_soft: rgb(0x13312a).into(),
            mint_fg: rgb(0x7ee0bd).into(),
            amber: rgb(0xe8a33f).into(),
            amber_soft: rgb(0x33260f).into(),
            amber_fg: rgb(0xf0c684).into(),
            danger: rgb(0xe2567c).into(),
            danger_soft: rgb(0x3a1a24).into(),
            danger_fg: rgb(0xf79bb0).into(),
            ink: rgb(0xf6ecf0).into(),
            ink_muted: rgb(0xc9aab6).into(),
            ink_placeholder: rgb(0xab8b98).into(),
            border: rgb(0x4a2f3a).into(),
            border_title: rgb(0x50333f).into(),
            border_float: rgb(0x6b4254).into(),
            shadow_tint: hsla(340. / 360., 0.60, 0.04, 0.45),
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

/// Picks a palette from an OS dark-mode flag.
///
/// The project has no OS-appearance signal yet: `ene-platform-os` exposes
/// capture, input, tray and window control but nothing that reports the
/// system colour scheme, and `gpui`'s `Window::appearance()` is unused here.
/// Rather than invent a platform call, this maps the flag a caller would
/// already have; wiring it into `Hub::theme` is a separate change.
// Remove this allow once a caller supplies the OS dark-mode flag.
#[allow(dead_code)]
pub fn for_os_appearance(dark: bool) -> Theme {
    if dark {
        Theme::midnight_berry()
    } else {
        Theme::strawberry_milk()
    }
}

/// Primary Japanese UI font family.
#[cfg_attr(any(target_os = "linux", target_os = "freebsd"), allow(dead_code))]
pub fn ui_font_family() -> &'static str {
    "Yu Gothic UI"
}

/// Resolves the UI font family for the current OS, picking an installed font on
/// Linux where GPUI 0.2.2 ignores `FontFallbacks`.
pub fn resolve_ui_font_family(cx: &gpui::App) -> gpui::SharedString {
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        let _discarded = cx;
        gpui::SharedString::new_static(ui_font_family())
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        static RESOLVED_FONT: std::sync::OnceLock<gpui::SharedString> = std::sync::OnceLock::new();
        RESOLVED_FONT
            .get_or_init(|| {
                let names = cx.text_system().all_font_names();
                pick_linux_ui_font(&names).into()
            })
            .clone()
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd", test))]
fn pick_linux_ui_font(available: &[String]) -> String {
    const LINUX_UI_FONTS: &[&str] = &[
        "Noto Sans CJK JP",
        "BIZ UDPGothic",
        "IBM Plex Sans JP",
        "IPAPGothic",
        "IPAGothic",
        "VL PGothic",
        "Hiragino Sans",
        "Noto Sans",
        "Ubuntu",
        "Adwaita Sans",
        "Cantarell",
        "DejaVu Sans",
    ];
    for candidate in LINUX_UI_FONTS {
        if available.iter().any(|name| name == candidate) {
            return (*candidate).to_owned();
        }
    }
    available
        .iter()
        .find(|name| !name.starts_with('.'))
        .cloned()
        .unwrap_or_else(|| ".SystemUIFont".to_owned())
}

pub fn ui_font_fallbacks() -> FontFallbacks {
    FontFallbacks::from_fonts(vec![
        "Hiragino Sans".to_owned(),
        "Meiryo".to_owned(),
        "BIZ UDPGothic".to_owned(),
        "Noto Sans CJK JP".to_owned(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_font_picker_prefers_japanese_ui_font_when_installed() {
        let fonts = vec![
            "DejaVu Sans".to_owned(),
            "Noto Sans CJK HK".to_owned(),
            "Noto Sans CJK JP".to_owned(),
        ];
        assert_eq!(pick_linux_ui_font(&fonts), "Noto Sans CJK JP");
    }

    #[test]
    fn linux_font_picker_falls_back_to_installed_non_hidden_family() {
        let fonts = vec![".SystemUIFont".to_owned(), "Liberation Sans".to_owned()];
        assert_eq!(pick_linux_ui_font(&fonts), "Liberation Sans");
    }

    /// WCAG 2.1 relative luminance of an opaque colour.
    fn relative_luminance(color: Hsla) -> f64 {
        let rgba = gpui::Rgba::from(color);
        let channel = |value: f32| {
            let value = f64::from(value);
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
    }

    /// WCAG 2.1 contrast ratio, always >= 1 for two opaque colours.
    fn contrast_ratio(a: Hsla, b: Hsla) -> f64 {
        let (lighter, darker) = {
            let (la, lb) = (relative_luminance(a), relative_luminance(b));
            if la >= lb { (la, lb) } else { (lb, la) }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn midnight_berry_text_reaches_aa_contrast() {
        let theme = Theme::midnight_berry();
        let pairs = [
            ("ink on bg_surface", theme.ink, theme.bg_surface),
            ("ink_muted on bg_surface", theme.ink_muted, theme.bg_surface),
            (
                "ink_placeholder on bg_input",
                theme.ink_placeholder,
                theme.bg_input,
            ),
            ("accent_fg on accent", theme.accent_fg, theme.accent),
            (
                "accent_fg on accent_hover",
                theme.accent_fg,
                theme.accent_hover,
            ),
            (
                "accent_soft_fg on accent_soft",
                theme.accent_soft_fg,
                theme.accent_soft,
            ),
            ("mint_fg on mint_soft", theme.mint_fg, theme.mint_soft),
            ("amber_fg on amber_soft", theme.amber_fg, theme.amber_soft),
            (
                "danger_fg on danger_soft",
                theme.danger_fg,
                theme.danger_soft,
            ),
            ("ink on bg_blush", theme.ink, theme.bg_blush),
            ("ink_muted on bg_blush", theme.ink_muted, theme.bg_blush),
            (
                "ink_placeholder on bg_blush",
                theme.ink_placeholder,
                theme.bg_blush,
            ),
            ("ink on bg_user_msg", theme.ink, theme.bg_user_msg),
            ("ink_muted on bg_milk", theme.ink_muted, theme.bg_milk),
        ];
        for (pair, fg, bg) in pairs {
            let ratio = contrast_ratio(fg, bg);
            assert!(ratio >= 4.5, "{pair} is {ratio:.2}:1, below 4.5:1");
        }
    }

    #[test]
    fn default_theme_stays_the_light_palette() {
        assert_eq!(Theme::default(), Theme::strawberry_milk());
        assert_eq!(for_os_appearance(false), Theme::strawberry_milk());
        assert_eq!(for_os_appearance(true), Theme::midnight_berry());
    }
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
