use super::selection::{
    MASK_CHAR, clamp_index, clamp_range, masked_offset, offset_from_utf16_in, single_line,
    unmasked_offset,
};

#[test]
fn a_stale_mark_cannot_panic() {
    assert_eq!(clamp_range("", 44..44), 0..0);
    assert_eq!(clamp_range("", 0..7), 0..0);
    assert_eq!(clamp_range("hi", 0..44), 0..2);
}

#[test]
fn a_range_lands_on_character_boundaries() {
    assert_eq!(clamp_range("あい", 1..2), 0..0);
    assert_eq!(clamp_range("あい", 0..1), 0..0);
    assert_eq!(clamp_range("あい", 3..6), 3..6);
    assert_eq!(clamp_range("ab", 1..2), 1..2);
}

#[test]
fn a_point_cannot_name_a_position_past_the_text() {
    assert_eq!(clamp_index("", 7), 0);
    assert_eq!(clamp_index("hi", 99), 2);
    assert_eq!(clamp_index("あ", 1), 0);
    assert_eq!(clamp_index("あ", 3), 3);
}

#[test]
fn clipboard_shortcuts_use_the_platform_modifier() {
    let paste = gpui::Keystroke::parse("secondary-v").expect("secondary-v parses");
    assert_eq!(paste.key, "v");
    assert!(paste.modifiers.secondary());
    assert!(!paste.modifiers.platform || cfg!(target_os = "macos"));

    let windows_key = gpui::Keystroke::parse("cmd-v").expect("cmd-v parses");
    assert!(windows_key.modifiers.platform);
    assert!(!windows_key.modifiers.secondary());
}

#[test]
fn masked_offsets_round_trip_utf8_boundaries() {
    let text = "aあbい";
    let bullet_len = MASK_CHAR.len_utf8();
    assert_eq!(masked_offset(text, 0), 0);
    assert_eq!(masked_offset(text, 1), bullet_len);
    assert_eq!(masked_offset(text, 4), 2 * bullet_len);
    assert_eq!(masked_offset(text, 5), 3 * bullet_len);
    assert_eq!(masked_offset(text, text.len()), 4 * bullet_len);
    assert_eq!(masked_offset(text, 2), bullet_len);

    assert_eq!(unmasked_offset(text, 0), 0);
    assert_eq!(unmasked_offset(text, bullet_len), 1);
    assert_eq!(unmasked_offset(text, 2 * bullet_len), 4);
    assert_eq!(unmasked_offset(text, 3 * bullet_len), 5);
    assert_eq!(unmasked_offset(text, 4 * bullet_len), text.len());
    assert_eq!(unmasked_offset(text, 99), text.len());
}

#[test]
fn single_line_and_utf16_selection_clamp_multiline_japanese_text() {
    let raw = "ユーザー：テスト\r\n本人：返答";
    let clean = single_line(raw);
    assert!(!clean.contains('\n') && !clean.contains('\r'), "{clean}");
    assert_eq!(clean, "ユーザー：テスト  本人：返答");

    let sample = "あいう";
    assert_eq!(offset_from_utf16_in(sample, 0), 0);
    assert_eq!(offset_from_utf16_in(sample, 2), 6);
    assert_eq!(offset_from_utf16_in(sample, 99), sample.len());
    assert_eq!(clamp_range(&clean, 0..999), 0..clean.len());
}
