//! Host application, execution policy and authorized provider communication.
mod application;
mod domain;
mod llm;
mod ports;

mod api;
mod bootstrap;
mod devices;
mod infrastructure;
mod rig;
#[cfg(test)]
mod test_support;

mod server;
pub use server::run;

/// Truncates `value` to at most `limit` Unicode scalar values without
/// splitting a grapheme cluster.
pub(crate) fn truncate_chars(value: &str, limit: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut result = String::new();
    let mut count = 0;
    for grapheme in value.graphemes(true) {
        let next = grapheme.chars().count();
        if count + next > limit {
            break;
        }
        result.push_str(grapheme);
        count += next;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn truncation_keeps_the_character_limit_and_grapheme_boundaries() {
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("e\u{301}x", 1), "");
        assert_eq!(truncate_chars("e\u{301}x", 2), "e\u{301}");
        assert_eq!(truncate_chars("a👨‍👩‍👧b", 2), "a");
        let flags = truncate_chars("🇯🇵🇺🇸", 3);
        assert_eq!(flags, "🇯🇵");
        assert_eq!(flags.chars().count(), 2);
        assert!(flags.chars().count() <= 3);
    }
}
