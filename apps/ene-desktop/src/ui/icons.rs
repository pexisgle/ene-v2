//! Product icons use the Kit's selected native SVG bundle.
use gpui::{AssetSource, Hsla, Result, SharedString, Svg, prelude::*, px, svg};
use std::borrow::Cow;

pub use gpui::assets::IconName as Icon;

gpui::assets::icon_assets!(
    SelectedIcons,
    [
        BookUser,
        Check,
        ChevronDown,
        ChevronRight,
        Close,
        Crosshair,
        Ellipsis,
        Eye,
        EyeOff,
        Loader,
        MessageCircle,
        Mic,
        MicOff,
        Minimize2,
        Plug,
        Plus,
        RefreshCw,
        Search,
        Send,
        Settings,
        SlidersVertical,
        Sparkles,
        SquareStop,
        Trash,
        Volume2,
        VolumeX,
        WindowMaximize,
        WindowMinimize,
    ]
);

#[derive(Debug)]
pub struct EmbeddedAssets;

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(icon) = SelectedIcons.load(path)? {
            return Ok(Some(icon));
        }
        gpui::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui::assets::Assets.list(path)?;
        paths.extend(SelectedIcons.list(path)?);
        paths.sort_unstable();
        paths.dedup();
        Ok(paths)
    }
}

pub fn icon(kind: Icon, px_size: f32, color: impl Into<Hsla>) -> Svg {
    svg()
        .path(kind.path())
        .size(px(px_size.max(8.)))
        .flex_none()
        .text_color(color.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_selected_product_icon_loads_valid_svg() {
        let assets = EmbeddedAssets;
        let paths = SelectedIcons.list("icons/").unwrap();
        assert_eq!(paths.len(), 28);
        for path in paths {
            let contents = assets.load(&path).unwrap().unwrap();
            let svg = std::str::from_utf8(&contents).unwrap().trim();
            assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"), "{path}");
        }
    }
}
