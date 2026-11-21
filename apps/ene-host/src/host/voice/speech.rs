//! Text preparation for companion replies:
//! - [`strip_motion_leak`] cleans only a leaked leading JSON animation array so
//!   chat transcripts, history and live previews preserve Markdown, URLs, code
//!   blocks and emojis verbatim (`B12`).
//! - [`for_speech`] produces natural spoken prose for TTS engines by reading
//!   tables as label-value clauses and removing raw URLs, fenced code blocks
//!   and emojis (`R11`).
use regex::Regex;
use std::sync::LazyLock;

static EMOJI: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"(?:[#*0-9]\u{fe0f}?\u{20e3}|[\p{Extended_Pictographic}\p{Emoji_Presentation}](?:\u{fe0f}|\p{Emoji_Modifier})*(?:\u{200d}[\p{Extended_Pictographic}\p{Emoji_Presentation}](?:\u{fe0f}|\p{Emoji_Modifier})*)*)").ok()
});

static BARE_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"https?://[^\s\)\]>」』）】]+").ok());

fn motion_line(line: &str) -> bool {
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

/// Removes only a leaked leading standalone JSON animation array while leaving
/// Markdown, code blocks, URLs and emojis intact for display and transcripts.
pub(crate) fn strip_motion_leak(raw: &str) -> String {
    let trimmed = raw.trim();
    let first = trimmed.lines().next().unwrap_or("");
    if motion_line(first) {
        trimmed[first.len()..].trim_start().to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Converts a companion reply into natural spoken text for TTS synthesis.
pub(crate) fn for_speech(raw: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let raw = strip_motion_leak(raw);
    let mut out = String::new();
    let mut headers: Vec<String> = Vec::new();
    let mut heading = false;
    let mut cell = 0;
    let mut start = 0;
    let mut code = false;
    for event in Parser::new_ext(&raw, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Start(Tag::Table(_)) => headers.clear(),
            Event::Start(Tag::TableHead) => heading = true,
            Event::End(TagEnd::TableHead) => heading = false,
            Event::Start(Tag::TableRow) => cell = 0,
            Event::Start(Tag::TableCell) => {
                start = out.len();
                if !heading && let Some(label) = headers.get(cell) {
                    if cell > 0 {
                        out.push('、');
                    }
                    out.push_str(label);
                    out.push('は');
                }
            }
            Event::End(TagEnd::TableCell) => {
                if heading {
                    headers.push(out.split_off(start));
                }
                cell += 1;
            }
            Event::End(TagEnd::TableRow) => out.push_str("。\n"),
            Event::Start(Tag::CodeBlock(_)) => code = true,
            Event::End(TagEnd::CodeBlock) => {
                code = false;
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
            }
            Event::Text(_) if code => {}
            Event::Text(text) => {
                let no_urls = match &*BARE_URL {
                    Some(re) => re.replace_all(&text, ""),
                    None => std::borrow::Cow::Borrowed(text.as_ref()),
                };
                let clean = match &*EMOJI {
                    Some(re) => re.replace_all(&no_urls, ""),
                    None => no_urls,
                };
                out.push_str(&clean);
            }
            Event::Code(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item)
                if !out.ends_with('\n') && !out.is_empty() =>
            {
                out.push('\n');
            }
            _ => {}
        }
    }
    out.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b12_strip_motion_leak_preserves_markdown_code_urls_and_emojis() {
        let reply = "[{\"motion\":\"nod\",\"expression\":\"happy\"}]\n\n\
            **おすすめ**だよ 👍🏽 [Rust公式サイト](https://www.rust-lang.org/)\n\
            ```rust\nfn main() { println!(\"hi\"); }\n```";
        let display = strip_motion_leak(reply);
        assert!(!display.contains(r#""motion":"nod""#));
        assert!(display.contains("**おすすめ**"));
        assert!(display.contains("👍🏽"));
        assert!(display.contains("[Rust公式サイト](https://www.rust-lang.org/)"));
        assert!(display.contains("```rust\nfn main()"));

        let literal_code = "```json\n[{\"motion\":\"nod\"}]\n```";
        assert_eq!(strip_motion_leak(literal_code), literal_code);
    }

    #[test]
    fn b12_for_speech_reads_tables_and_strips_urls_code_and_emojis() {
        let reply = "[{\"motion\":\"nod\",\"expression\":\"happy\"}]\n\n\
            Aがおすすめです。👍🏽 詳しくは [公式サイト](https://example.org/docs) や https://example.org/raw を見てね。\n\
            |候補|横幅|価格|\n|---|---:|---:|\n|A|53センチ|2万円|\n|B|60センチ|3万円|\n\n\
            ```rust\nfn secret_code() {}\n```";
        let spoken = for_speech(reply);
        assert!(spoken.contains("Aがおすすめです。"));
        assert!(spoken.contains("公式サイト"));
        assert!(!spoken.contains("https://"));
        assert!(!spoken.contains("👍"));
        assert!(!spoken.contains("secret_code"));
        assert!(spoken.contains("候補はA、横幅は53センチ、価格は2万円。"));
        assert!(spoken.contains("候補はB、横幅は60センチ、価格は3万円。"));
    }
}
