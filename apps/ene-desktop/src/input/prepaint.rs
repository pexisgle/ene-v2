//! Turning a [`Draft`]'s text into shaped lines and caret/selection quads.
//!
//! Split out of `render.rs`; `FieldElement` still owns layout, paint and the
//! input handler, and hands the display state prepared here over to paint.

use super::render::{FieldLayout, wrapped_position};
use super::selection::{MASK_CHAR, clamp_index, clamp_range, masked_offset, single_line};
use super::{Draft, FieldLook};
use gpui::{
    Bounds, PaintQuad, Pixels, SharedString, TextRun, UnderlineStyle, Window, WrappedLine, fill,
    point, px, size,
};
use std::ops::Range;

pub(super) fn shape_wrapped(
    window: &Window,
    text: SharedString,
    font_size: Pixels,
    runs: &[TextRun],
    width: Option<Pixels>,
) -> Vec<WrappedLine> {
    window
        .text_system()
        .shape_text(text, font_size, runs, width, None)
        .unwrap_or_default()
        .into_vec()
}

pub(super) struct Prepaint {
    pub(super) layout: Option<FieldLayout>,
    pub(super) cursor: Option<PaintQuad>,
    pub(super) selection: Option<PaintQuad>,
    pub(super) offset: Pixels,
}

pub(super) struct PreparedText {
    text: SharedString,
    selected: Range<usize>,
    cursor: usize,
    run: TextRun,
    runs: Vec<TextRun>,
    look: FieldLook,
    pub(super) multiline: bool,
}

pub(super) fn prepare_display_text(input: &Draft, style: &gpui::TextStyle) -> PreparedText {
    let content = input.content.clone();
    let masked = input.masked && !content.is_empty();
    let look = input.look;
    let (text, color) = if content.is_empty() {
        (input.placeholder.clone(), look.placeholder)
    } else if masked {
        let bullets: String = std::iter::repeat_n(MASK_CHAR, content.chars().count()).collect();
        (SharedString::from(bullets), look.text)
    } else {
        (content.clone(), look.text)
    };
    let text = if !input.multiline() && text.contains(['\r', '\n']) {
        SharedString::from(single_line(&text))
    } else {
        text
    };
    let selected = if content.is_empty() {
        0..0
    } else if masked {
        clamp_range(
            &text,
            masked_offset(&content, input.selected.start)
                ..masked_offset(&content, input.selected.end),
        )
    } else {
        clamp_range(&text, input.selected.clone())
    };
    let cursor = if content.is_empty() {
        0
    } else if masked {
        clamp_index(&text, masked_offset(&content, input.cursor()))
    } else {
        clamp_index(&text, input.cursor())
    };
    let marked = (!content.is_empty())
        .then(|| {
            input.marked.as_ref().and_then(|range| {
                let clamped = if masked {
                    clamp_range(
                        &text,
                        masked_offset(&content, range.start)..masked_offset(&content, range.end),
                    )
                } else {
                    clamp_range(&text, range.clone())
                };
                (!clamped.is_empty()).then_some(clamped)
            })
        })
        .flatten();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = build_text_runs(
        &text,
        &run,
        &selected,
        marked.as_ref(),
        look,
        input.multiline(),
    );
    PreparedText {
        text,
        selected,
        cursor,
        run,
        runs,
        look,
        multiline: input.multiline(),
    }
}
fn build_text_runs(
    text: &str,
    run: &TextRun,
    selected: &Range<usize>,
    marked: Option<&Range<usize>>,
    look: FieldLook,
    multiline: bool,
) -> Vec<TextRun> {
    let mut boundaries = vec![0, text.len()];
    if let Some(marked) = marked {
        boundaries.extend([marked.start, marked.end]);
    }
    if multiline && !selected.is_empty() {
        boundaries.extend([selected.start, selected.end]);
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .map(|span| TextRun {
            len: span[1] - span[0],
            background_color: (multiline && selected.contains(&span[0])).then_some(look.selection),
            underline: marked
                .is_some_and(|range| range.contains(&span[0]))
                .then_some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.),
                    wavy: false,
                }),
            ..run.clone()
        })
        .collect()
}

pub(super) fn prepaint_multiline(
    prepared: PreparedText,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) -> Prepaint {
    let font_size = window.text_style().font_size.to_pixels(window.rem_size());
    let height = window.line_height();
    let lines = shape_wrapped(
        window,
        prepared.text,
        font_size,
        &prepared.runs,
        Some(bounds.size.width),
    );
    let caret = wrapped_position(&lines, height, prepared.cursor).unwrap_or_default();
    Prepaint {
        layout: Some(FieldLayout::Multiline(lines, height)),
        cursor: prepared.selected.is_empty().then(|| {
            fill(
                Bounds::new(
                    bounds.origin + caret - point(px(1.), px(0.)),
                    size(px(2.), height),
                ),
                prepared.look.caret,
            )
        }),
        selection: None,
        offset: px(0.),
    }
}

pub(super) fn prepaint_single_line(
    prepared: PreparedText,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) -> Prepaint {
    let font_size = window.text_style().font_size.to_pixels(window.rem_size());
    let line = window.text_system().shape_line(
        prepared.text,
        font_size,
        if prepared.runs.is_empty() {
            std::slice::from_ref(&prepared.run)
        } else {
            &prepared.runs
        },
        None,
    );
    let caret = line.x_for_index(prepared.cursor);
    let offset = (caret - bounds.size.width + px(4.)).max(px(0.));
    let caret_shift = if prepared.cursor == 0 { px(3.) } else { px(1.) };
    let (selection, cursor) = if prepared.selected.is_empty() {
        (
            None,
            Some(fill(
                Bounds::new(
                    point(bounds.left() + caret - offset - caret_shift, bounds.top()),
                    size(px(2.), bounds.bottom() - bounds.top()),
                ),
                prepared.look.caret,
            )),
        )
    } else {
        (
            Some(fill(
                Bounds::from_corners(
                    point(
                        bounds.left() + line.x_for_index(prepared.selected.start) - offset,
                        bounds.top(),
                    ),
                    point(
                        bounds.left() + line.x_for_index(prepared.selected.end) - offset,
                        bounds.bottom(),
                    ),
                ),
                prepared.look.selection,
            )),
            None,
        )
    };
    Prepaint {
        layout: Some(FieldLayout::Single(Box::new(line))),
        cursor,
        selection,
        offset,
    }
}
