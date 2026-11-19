//! In-memory SVG icon assets loaded by GPUI's `AssetSource`.

use gpui::{AssetSource, Hsla, Result, SharedString, Svg, prelude::*, px, svg};
use std::borrow::Cow;

/// Provides embedded SVG icons to GPUI without reading external files at runtime.
pub struct EmbeddedAssets;

/// Drawn SVG icon identifiers used across the pod and settings UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Mic,
    MicOff,
    Volume,
    VolumeOff,
    Stop,
    Send,
    Sparkles,
    ChatBubble,
    ChatExpand,
    ChatCollapse,
    Settings,
    More,
    ChevronDown,
    ChevronUp,
    ChevronRight,
    Search,
    Plus,
    Check,
    Close,
    Minimize,
    Maximize,
    Heart,
    UserBook,
    Sliders,
    Plug,
    Shield,
    Eye,
    EyeOff,
    Refresh,
    Crosshair,
    Trash,
}

impl Icon {
    pub const ALL: [Icon; 31] = [
        Icon::Mic,
        Icon::MicOff,
        Icon::Volume,
        Icon::VolumeOff,
        Icon::Stop,
        Icon::Send,
        Icon::Sparkles,
        Icon::ChatBubble,
        Icon::ChatExpand,
        Icon::ChatCollapse,
        Icon::Settings,
        Icon::More,
        Icon::ChevronDown,
        Icon::ChevronUp,
        Icon::ChevronRight,
        Icon::Search,
        Icon::Plus,
        Icon::Check,
        Icon::Close,
        Icon::Minimize,
        Icon::Maximize,
        Icon::Heart,
        Icon::UserBook,
        Icon::Sliders,
        Icon::Plug,
        Icon::Shield,
        Icon::Eye,
        Icon::EyeOff,
        Icon::Refresh,
        Icon::Crosshair,
        Icon::Trash,
    ];

    pub const fn path(self) -> &'static str {
        match self {
            Icon::Mic => "icons/mic.svg",
            Icon::MicOff => "icons/mic_off.svg",
            Icon::Volume => "icons/volume.svg",
            Icon::VolumeOff => "icons/volume_off.svg",
            Icon::Stop => "icons/stop.svg",
            Icon::Send => "icons/send.svg",
            Icon::Sparkles => "icons/sparkles.svg",
            Icon::ChatBubble => "icons/chat_bubble.svg",
            Icon::ChatExpand => "icons/chat_expand.svg",
            Icon::ChatCollapse => "icons/chat_collapse.svg",
            Icon::Settings => "icons/settings.svg",
            Icon::More => "icons/more.svg",
            Icon::ChevronDown => "icons/chevron_down.svg",
            Icon::ChevronUp => "icons/chevron_up.svg",
            Icon::ChevronRight => "icons/chevron_right.svg",
            Icon::Search => "icons/search.svg",
            Icon::Plus => "icons/plus.svg",
            Icon::Check => "icons/check.svg",
            Icon::Close => "icons/close.svg",
            Icon::Minimize => "icons/minimize.svg",
            Icon::Maximize => "icons/maximize.svg",
            Icon::Heart => "icons/heart.svg",
            Icon::UserBook => "icons/user_book.svg",
            Icon::Sliders => "icons/sliders.svg",
            Icon::Plug => "icons/plug.svg",
            Icon::Shield => "icons/shield.svg",
            Icon::Eye => "icons/eye.svg",
            Icon::EyeOff => "icons/eye_off.svg",
            Icon::Refresh => "icons/refresh.svg",
            Icon::Crosshair => "icons/crosshair.svg",
            Icon::Trash => "icons/trash.svg",
        }
    }

    const fn svg_bytes(self) -> &'static [u8] {
        match self {
            Icon::More => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="#000000"><circle cx="5" cy="12" r="2"/><circle cx="12" cy="12" r="2"/><circle cx="19" cy="12" r="2"/></svg>"##,
            Icon::Mic => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="2" width="6" height="12" rx="3"/><path d="M5 10v2a7 7 0 0 0 14 0v-2"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="8" y1="22" x2="16" y2="22"/></svg>"##,
            Icon::MicOff => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="2" y1="2" x2="22" y2="22"/><path d="M15 9.34V5a3 3 0 0 0-5.68-1.33"/><path d="M9 9v3a3 3 0 0 0 5.12 2.12"/><path d="M17 16.95A7 7 0 0 1 5 12v-2"/><path d="M19 10v2a7 7 0 0 1-.11 1.23"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="8" y1="22" x2="16" y2="22"/></svg>"##,
            Icon::Volume => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><path d="M15.54 8.46a5 5 0 0 1 0 7.07"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/></svg>"##,
            Icon::VolumeOff => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5"/><line x1="22" y1="9" x2="16" y2="15"/><line x1="16" y1="9" x2="22" y2="15"/></svg>"##,
            Icon::Stop => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="5" width="14" height="14" rx="3" fill="#000000"/></svg>"##,
            Icon::Send => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2 11 13"/><path d="M22 2 15 22 11 13 2 9z"/></svg>"##,
            Icon::Sparkles => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l1.9 5.2L19 10l-5.1 1.8L12 17l-1.9-5.2L5 10l5.1-1.8L12 3z"/><path d="M19 16l.9 2.1L22 19l-2.1.9L19 22l-.9-2.1L16 19l2.1-.9L19 16z"/></svg>"##,
            Icon::ChatBubble => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a8 8 0 0 1-8 8H5l-2 2 1-4a8 8 0 1 1 17-6z"/></svg>"##,
            Icon::ChatExpand => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="15 3 21 3 21 9"/><polyline points="9 21 3 21 3 15"/><line x1="21" y1="3" x2="14" y2="10"/><line x1="3" y1="21" x2="10" y2="14"/></svg>"##,
            Icon::ChatCollapse => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="4 14 10 14 10 20"/><polyline points="20 10 14 10 14 4"/><line x1="14" y1="10" x2="21" y2="3"/><line x1="3" y1="21" x2="10" y2="14"/></svg>"##,
            Icon::Settings => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/></svg>"##,
            Icon::ChevronDown => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="6 9 12 15 18 9"/></svg>"##,
            Icon::ChevronUp => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="18 15 12 9 6 15"/></svg>"##,
            Icon::ChevronRight => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><polyline points="9 6 15 12 9 18"/></svg>"##,
            Icon::Search => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="7"/><line x1="21" y1="21" x2="16.65" y2="16.65"/></svg>"##,
            Icon::Plus => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/></svg>"##,
            Icon::Check => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><polyline points="20 6 9 17 4 12"/></svg>"##,
            Icon::Close => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>"##,
            Icon::Minimize => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2.2" stroke-linecap="round"><line x1="5" y1="12" x2="19" y2="12"/></svg>"##,
            Icon::Maximize => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="5" width="14" height="14" rx="2.5"/></svg>"##,
            Icon::Heart => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M20.84 4.61a5.5 5.5 0 0 0-7.78 0L12 5.67l-1.06-1.06a5.5 5.5 0 0 0-7.78 7.78l1.06 1.06L12 21.23l7.78-7.78 1.06-1.06a5.5 5.5 0 0 0 0-7.78z"/></svg>"##,
            Icon::UserBook => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/><circle cx="12" cy="9" r="2.2"/><path d="M8.5 14.5a3.5 3.5 0 0 1 7 0"/></svg>"##,
            Icon::Sliders => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="1" y1="14" x2="7" y2="14"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="17" y1="16" x2="23" y2="16"/></svg>"##,
            Icon::Plug => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a6 6 0 0 1-12 0V8z"/></svg>"##,
            Icon::Shield => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/></svg>"##,
            Icon::Eye => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></svg>"##,
            Icon::EyeOff => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M17.94 17.94A10.07 10.07 0 0 1 12 20c-7 0-11-8-11-8a18.45 18.45 0 0 1 5.06-5.94"/><path d="M9.9 4.24A9.12 9.12 0 0 1 12 4c7 0 11 8 11 8a18.5 18.5 0 0 1-2.16 3.19"/><line x1="1" y1="1" x2="23" y2="23"/></svg>"##,
            Icon::Refresh => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="23 4 23 10 17 10"/><path d="M20.49 15a9 9 0 1 1-2.12-9.36L23 10"/></svg>"##,
            Icon::Crosshair => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><line x1="22" y1="12" x2="18" y2="12"/><line x1="6" y1="12" x2="2" y2="12"/><line x1="12" y1="6" x2="12" y2="2"/><line x1="12" y1="22" x2="12" y2="18"/></svg>"##,
            Icon::Trash => br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><polyline points="3 6 5 6 21 6"/><path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"/><path d="M10 11v6"/><path d="M14 11v6"/><path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/></svg>"##,
        }
    }
}

impl AssetSource for EmbeddedAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let found = Icon::ALL
            .into_iter()
            .find(|icon| icon.path() == path)
            .map(|icon| Cow::Borrowed(icon.svg_bytes()));
        Ok(found)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .into_iter()
            .filter(|icon| icon.path().starts_with(path))
            .map(|icon| SharedString::from(icon.path()))
            .collect())
    }
}

/// Renders an embedded SVG icon at a non-zero square size.
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
    fn every_embedded_icon_loads_valid_svg() {
        let assets = EmbeddedAssets;
        for kind in Icon::ALL {
            let bytes = assets
                .load(kind.path())
                .expect("asset load does not error")
                .expect("every icon path resolves");
            let text = std::str::from_utf8(&bytes).expect("svg is utf8");
            assert!(
                text.starts_with("<svg ") && text.ends_with("</svg>"),
                "{} is not an svg document",
                kind.path()
            );
        }
        assert_eq!(assets.list("icons/").unwrap().len(), Icon::ALL.len());
    }
}
