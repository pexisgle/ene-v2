//! Text replies in the companion pod.

use crate::state::Model;
use ene_client_core::domain::{MessageRole, TurnStatus};
use gpui::{ElementId, ImageSource, base::text::TextView};
use std::sync::Arc;
use url::Url;

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

pub(super) fn reply_view(id: impl Into<ElementId>, text: String) -> TextView {
    TextView::markdown(id, text)
        .markdown_raw_html(false)
        // Conversation data cannot cause an automatic image fetch or file read.
        .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
        .link(|link, _| web_link(&link.url).map(|url| link.clone().url(url.as_str().to_owned())))
}

fn web_link(raw: &str) -> Option<Url> {
    let url = Url::parse(raw.trim_end_matches(['。', '、', '」', '』'])).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
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
    fn preview_preserves_reply_markup() {
        let raw = "### 結果\n- **公式**: [出典](https://example.com/docs) を確認 👍";
        assert_eq!(preview_text(raw), raw);
    }

    #[test]
    fn citations_open_only_explicit_web_urls() {
        assert_eq!(
            web_link("https://example.com/docs。").unwrap().as_str(),
            "https://example.com/docs"
        );
        for value in [
            "file:///tmp/example",
            "javascript:alert(1)",
            "https://user@host/",
            "/local",
        ] {
            assert!(web_link(value).is_none());
        }
    }
}
