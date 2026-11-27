//! Text replies in the companion pod.

use crate::state::Model;
use ene_client_core::domain::{MessageRole, TurnStatus};
use ene_voice::speech::strip_motion_leak;
use gpui::{ElementId, ImageSource, base::text::TextView};
use std::sync::Arc;
use url::Url;

pub(super) fn streaming_text(model: &Model) -> String {
    let id = model.companions.chat.companion_id.as_ref();
    model
        .companions
        .streaming
        .iter()
        .filter(|((companion, _), _)| id.is_some_and(|id| companion == id.as_str()))
        .map(|(_, text)| text.as_str())
        .collect()
}

pub(crate) fn active_mini_speech_text(model: &Model) -> String {
    let streaming = streaming_text(model);
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

pub(crate) fn preview_text(raw: &str) -> String {
    // A leading JSON array may be a motion cue. A Markdown link can start
    // with the same bracket, so hold only a plausible unfinished JSON array.
    let trimmed = raw.trim();
    if !trimmed.contains('\n')
        && trimmed.strip_prefix('[').is_some_and(|rest| {
            let rest = rest.trim_start();
            rest.is_empty() || rest.starts_with('{')
        })
        && serde_json::from_str::<serde_json::Value>(trimmed).is_err_and(|error| error.is_eof())
    {
        return String::new();
    }
    strip_motion_leak(raw)
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
        let link = "[出典](https://example.com/docs)";
        assert_eq!(preview_text(link), link);
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
