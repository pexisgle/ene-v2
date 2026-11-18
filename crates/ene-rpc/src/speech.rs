use regex::Regex;
use std::sync::LazyLock;

// Apply the same fallback to completed speech, the transcript and live previews.
static EMOJI: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:[#*0-9]\u{fe0f}?\u{20e3}|[\p{Extended_Pictographic}\p{Emoji_Presentation}](?:\u{fe0f}|\p{Emoji_Modifier})*(?:\u{200d}[\p{Extended_Pictographic}\p{Emoji_Presentation}](?:\u{fe0f}|\p{Emoji_Modifier})*)*)").unwrap()
});

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

/// Plain text shared by conversation history, live previews and speech.
pub fn plain_reply(raw: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut out = String::new();
    let mut headers: Vec<String> = Vec::new();
    let mut heading = false;
    let mut cell = 0;
    let mut start = 0;
    let mut link = None;
    let mut code = false;
    // A standalone animation array before the greeting is a known model leak.
    let raw = raw.trim();
    let first = raw.lines().next().unwrap_or("");
    let raw = if motion_line(first) {
        raw[first.len()..].trim_start()
    } else {
        raw
    };
    for event in Parser::new_ext(raw, Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH) {
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
            Event::Start(Tag::Link { dest_url, .. }) => link = Some((out.len(), dest_url)),
            Event::End(TagEnd::Link) => {
                if let Some((start, url)) = link.take()
                    && (url.starts_with("https://") || url.starts_with("http://"))
                    && &out[start..] != url.as_ref()
                {
                    out.push_str(" (");
                    out.push_str(&url);
                    out.push(')');
                }
            }
            Event::Start(Tag::CodeBlock(_)) => code = true,
            Event::End(TagEnd::CodeBlock) => {
                code = false;
                out.push('\n');
            }
            Event::Text(text) if code => out.push_str(&text),
            Event::Text(text) => out.push_str(&EMOJI.replace_all(&text, "")),
            Event::Code(text) => out.push_str(&text),
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::End(TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item)
                if !out.ends_with('\n') =>
            {
                out.push('\n')
            }
            _ => {}
        }
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaks_values_with_their_labels_and_preserves_literal_code() {
        let reply = "[{\"motion\":\"nod\",\"expression\":\"happy\"}]\n\nAがおすすめです。👍🏽\n|候補|横幅|価格|\n|---|---:|---:|\n|A|53センチ|2万円|\n|B|60センチ|3万円|";
        assert_eq!(
            plain_reply(reply),
            "Aがおすすめです。\n候補はA、横幅は53センチ、価格は2万円。\n候補はB、横幅は60センチ、価格は3万円。"
        );
        let literal = "```json\n[{\"motion\":\"nod\"}]\n```";
        assert_eq!(plain_reply(literal), r#"[{"motion":"nod"}]"#);
    }
}
