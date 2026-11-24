//! Text extracted from a message for the pod: previews and URLs.
//!
//! Pure string work, so it is testable without a window and separable from
//! the element tree in `mod.rs`.

//! The companion pod's message stage.

use crate::state::Model;
use crate::ui::theme::Theme;
use ene_client_core::domain::{MessageRole, TurnStatus};
use gpui::{AnyElement, SharedString, div, prelude::*, px};

pub(crate) fn active_mini_speech_text(model: &Model) -> String {
    let streaming: String = model
        .companions
        .streaming
        .iter()
        .filter(|((c, _), _)| {
            model
                .companions
                .chat
                .companion_id
                .as_ref()
                .is_some_and(|id| c == id.as_str())
        })
        .map(|(_, text)| text.as_str())
        .collect();
    if !streaming.is_empty() {
        return streaming;
    }
    if !model.companions.chat.sending.is_empty()
        || model
            .companions
            .chat
            .turn
            .as_ref()
            .is_some_and(|t| t.status == TurnStatus::Streaming)
    {
        return "返答を考えています…".to_owned();
    }
    model
        .companions
        .chat
        .messages
        .iter()
        .rev()
        .find(|m| m.role != MessageRole::User)
        .map_or_else(
            || "こんにちは。まずはひとこと送ってみてください。".to_owned(),
            |m| m.text.clone(),
        )
}

fn is_motion_leak_line(line: &str) -> bool {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(line) else {
        return false;
    };
    !items.is_empty()
        && items.iter().all(|item| {
            item.as_object().is_some_and(|obj| {
                !obj.is_empty()
                    && obj.iter().all(|(key, value)| {
                        matches!(key.as_str(), "motion" | "expression") && value.is_string()
                    })
            })
        })
}

fn strip_motion_leak(raw: &str) -> String {
    let trimmed = raw.trim();
    let first = trimmed.lines().next().unwrap_or("");
    if is_motion_leak_line(first) {
        trimmed[first.len()..].trim_start().to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub(crate) fn preview_text(raw: &str) -> String {
    // Hold an unfinished leading motion-control JSON line until its shape is known,
    // while preserving Markdown, URLs, code blocks and emojis verbatim.
    let (complete, tail) = raw.rsplit_once('\n').unwrap_or(("", raw));
    if complete.is_empty() && tail.trim_start().starts_with('[') {
        return String::new();
    }
    if tail.trim_start().starts_with('[') && !tail.trim_end().ends_with(']') {
        strip_motion_leak(complete)
    } else {
        strip_motion_leak(raw)
    }
}

fn short_url_label(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let trimmed = without_scheme.trim_end_matches('/');
    if trimmed.chars().count() <= 32 {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(29).collect();
        format!("{head}…")
    }
}

pub(crate) fn extract_urls(raw: &str) -> Vec<(String, String)> {
    let mut urls: Vec<(String, String)> = Vec::new();
    collect_markdown_links(raw, &mut urls);
    collect_bare_urls(raw, &mut urls);
    urls
}

fn push_unique_url(urls: &mut Vec<(String, String)>, label: String, url: String) {
    if urls.len() >= 4 || urls.iter().any(|(_, existing)| existing == &url) {
        return;
    }
    let clean_label = if label.trim().is_empty() {
        short_url_label(&url)
    } else {
        label
    };
    urls.push((clean_label, url));
}

fn collect_markdown_links(raw: &str, urls: &mut Vec<(String, String)>) {
    let mut rest = raw;
    while let Some(open) = rest.find('[') {
        let after_open = &rest[open + 1..];
        if let Some(mid) = after_open.find("](")
            && let Some(close) = after_open[mid + 2..].find(')')
        {
            let label = after_open[..mid].trim();
            let url = after_open[mid + 2..mid + 2 + close].trim();
            if url.starts_with("http://") || url.starts_with("https://") {
                push_unique_url(urls, label.to_owned(), url.to_owned());
                rest = &after_open[mid + 2 + close + 1..];
                continue;
            }
        }
        rest = after_open;
    }
}

fn collect_bare_urls(raw: &str, urls: &mut Vec<(String, String)>) {
    for token in raw.split_whitespace() {
        let candidate = token.trim_start_matches(['(', '[', '<', '"', '\'', '「', '『']);
        let Some(pos) = candidate
            .find("https://")
            .or_else(|| candidate.find("http://"))
        else {
            continue;
        };
        let slice = &candidate[pos..];
        let end = slice
            .find(|ch: char| {
                matches!(
                    ch,
                    ')' | ']' | '>' | '"' | '\'' | ',' | '。' | '、' | '」' | '』'
                ) || ch.is_whitespace()
            })
            .unwrap_or(slice.len());
        let clean = slice[..end].trim_end_matches(['.', ';', ':']);
        if clean.starts_with("http://") || clean.starts_with("https://") {
            push_unique_url(urls, short_url_label(clean), clean.to_owned());
        }
    }
}

pub(crate) fn render_url_pills(raw: &str, id_prefix: &str, theme: &Theme) -> Option<AnyElement> {
    let urls = extract_urls(raw);
    if urls.is_empty() {
        return None;
    }
    let chips: Vec<AnyElement> = urls
        .into_iter()
        .enumerate()
        .map(|(idx, (label, url))| {
            let hover_bg = theme.accent_soft;
            let open_url = url.clone();
            div()
                .id(SharedString::from(format!("{id_prefix}-url-{idx}")))
                .rounded_full()
                .bg(theme.accent_soft)
                .border_1()
                .border_color(theme.border)
                .px(px(8.))
                .py(px(2.))
                .text_xs()
                .text_color(theme.accent_soft_fg)
                .cursor_pointer()
                .hover(move |style| style.bg(hover_bg))
                .on_click(move |_, _, cx| {
                    cx.open_url(&open_url);
                })
                .child(label)
                .into_any_element()
        })
        .collect();
    Some(
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(6.))
            .children(chips)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_holds_partial_motion_data_and_preserves_markdown() {
        assert_eq!(preview_text("[{\"motion\":"), "");
        assert_eq!(
            preview_text("[{\"motion\":\"nod\"}]\nこんにちは！"),
            "こんにちは！"
        );
    }

    #[test]
    fn preview_preserves_markdown_links_and_extract_urls_parses_them() {
        let raw = "### 結果\n- **公式**: [Antigravity](https://example.com/docs) を確認 👍\nhttps://example.com/raw。";
        let clean = preview_text(raw);
        assert!(clean.contains("**公式**"), "{clean}");
        assert!(
            clean.contains("[Antigravity](https://example.com/docs)"),
            "{clean}"
        );
        assert!(clean.contains("👍"), "{clean}");
        let urls = extract_urls(raw);
        assert_eq!(
            urls,
            vec![
                (
                    "Antigravity".to_owned(),
                    "https://example.com/docs".to_owned()
                ),
                (
                    "example.com/raw".to_owned(),
                    "https://example.com/raw".to_owned()
                ),
            ]
        );
    }
}
