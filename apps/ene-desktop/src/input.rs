//! One editable text field, built on the input handler GPUI ships.
//!
//! GPUI 0.2.2 has no text field of its own: an editor implements
//! `EntityInputHandler`, paints a `TextElement` and calls `Window::handle_input`
//! during paint. This module is that implementation, adapted from the crate's
//! `examples/input.rs`, and it is the only reason composition (Japanese IME)
//! works: the platform delivers composition through
//! `replace_and_mark_text_in_range`, and the marked range is drawn underlined.
//!
//! It is single-line on purpose. Newlines are pasted as spaces, `Enter` submits
//! through a key binding in `ChatInput` context, and a send is refused while a
//! composition is open so conversion is never cut short.

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hsla, InspectorElementId,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, Render, ShapedLine, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window, div, fill, point, prelude::*, px, relative, rgb, rgba, size,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

gpui::actions!(
    text_field,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Paste,
        Cut,
        Copy,
    ]
);

/// Visual palette for a [`Draft`] input field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldLook {
    pub bg: Hsla,
    pub border: Hsla,
    pub border_focused: Hsla,
    pub text: Hsla,
    pub placeholder: Hsla,
    pub caret: Hsla,
    pub selection: Hsla,
    pub pill: bool,
    pub borderless: bool,
}

impl Default for FieldLook {
    fn default() -> Self {
        Self {
            bg: rgb(0xffffff).into(),
            border: rgb(0xf3c4d5).into(),
            border_focused: rgb(0xcf2e68).into(),
            text: rgb(0x3b232c).into(),
            placeholder: rgb(0x7d4b5e).into(),
            caret: rgb(0xcf2e68).into(),
            selection: rgba(0xcf2e6833).into(),
            pill: false,
            borderless: false,
        }
    }
}

/// An editable field. One entity per input the window shows.
pub struct Draft {
    focus: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    /// The key context the field declares, so key bindings can be per-field.
    context: &'static str,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    /// Horizontal scroll in pixels, so the caret stays inside the field when the
    /// text is wider than it.
    scroll: Pixels,
    selecting: bool,
    masked: bool,
    look: FieldLook,
}

impl Draft {
    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        context: &'static str,
    ) -> Self {
        Self {
            focus: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            context,
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: None,
            last_bounds: None,
            scroll: px(0.),
            selecting: false,
            masked: false,
            look: FieldLook::default(),
        }
    }

    /// Enables or disables bullet masking (for secrets / API keys).
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.masked != masked {
            self.masked = masked;
            cx.notify();
        }
    }

    /// Whether the field hides its characters behind bullets.
    pub fn is_masked(&self) -> bool {
        self.masked
    }

    /// Updates the field's visual theme without re-notifying if unchanged.
    pub fn apply_look(&mut self, look: FieldLook) {
        self.look = look;
    }

    pub fn look(&self) -> &FieldLook {
        &self.look
    }

    /// Updates the hint shown while the field is empty.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// The text as it stands, including an open composition.
    pub fn text(&self) -> &str {
        &self.content
    }

    /// Whether an IME composition is open. A submit during composition would
    /// cut the conversion short, so callers check this first.
    pub fn composing(&self) -> bool {
        self.marked.is_some()
    }

    /// The field's focus handle, for focusing it when a panel opens.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Replaces the text and puts the caret at the end.
    pub fn set(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.selected = self.content.len()..self.content.len();
        self.marked = None;
        cx.notify();
    }

    /// Empties the field. The old text is the caller's if it wants it.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set("", cx);
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for character in self.content.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += character.len_utf16();
            utf8 += character.len_utf8();
        }
        utf8
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for character in self.content.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += character.len_utf8();
            utf16 += character.len_utf16();
        }
        utf16
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.reversed {
            self.selected.start = offset;
        } else {
            self.selected.end = offset;
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .as_str()
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let previous = self.previous_boundary(self.cursor());
            self.move_to(previous, cx);
        } else {
            let start = self.selected.start;
            self.move_to(start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.move_to(next, cx);
        } else {
            let end = self.selected.end;
            self.move_to(end, cx);
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let previous = self.previous_boundary(self.cursor());
        self.select_to(previous, cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_boundary(self.cursor());
        self.select_to(next, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        let end = self.content.len();
        self.select_to(end, cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let end = self.content.len();
        self.move_to(end, cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let previous = self.previous_boundary(self.cursor());
            self.select_to(previous, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &single_line(&text), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A click is also what gives a text field the keyboard. Without this the
        // platform focus stays wherever it was, and after any click elsewhere the
        // field never receives another key.
        window.focus(&self.focus);
        self.selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        // The shaped line is the placeholder while the field is empty, so a
        // click would otherwise report an index measured against the hint and
        // leave the caret past the end of the (empty) text.
        if self.content.is_empty() {
            return 0;
        }
        let shaped_index = line.closest_index_for_x(position.x - bounds.left() + self.scroll);
        if self.masked {
            unmasked_offset(&self.content, shaped_index)
        } else {
            clamp_index(&self.content, shaped_index)
        }
    }
}

const MASK_CHAR: char = '•';

/// Converts a UTF-8 byte offset in `content` to the byte offset in its
/// bullet-masked representation (`MASK_CHAR` repeated per scalar).
fn masked_offset(content: &str, byte_idx: usize) -> usize {
    let clamped = clamp_index(content, byte_idx);
    content[..clamped].chars().count() * MASK_CHAR.len_utf8()
}

/// Converts a UTF-8 byte offset in the bullet-masked string back to a valid
/// UTF-8 byte offset in `content`.
fn unmasked_offset(content: &str, masked_byte_idx: usize) -> usize {
    let char_index = masked_byte_idx / MASK_CHAR.len_utf8();
    content
        .char_indices()
        .nth(char_index)
        .map_or(content.len(), |(byte_idx, _)| byte_idx)
}

/// Collapses a paste into one line, so a pasted block stays a message.
fn single_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// A byte index inside `text`, on a character boundary.
///
/// The shaped line can be the placeholder (empty field) or extend past the last
/// glyph of the text, so an index computed from its geometry is not guaranteed
/// to name a position the text actually has.
fn clamp_index(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
/// A byte range clamped into `text` on character boundaries.
///
/// An IME range can outlive the text it was measured against: the field is
/// cleared or replaced while a composition is still open, and the stale range
/// then names bytes that no longer exist. Slicing there would panic, so every
/// range the input handler slices with passes through here first.
fn clamp_range(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let floor = |mut index: usize| {
        index = index.min(text.len());
        while index > 0 && !text.is_char_boundary(index) {
            index -= 1;
        }
        index
    };
    let start = floor(range.start);
    let end = floor(range.end.max(start));
    start..end
}
impl EntityInputHandler for Draft {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let range = clamp_range(&self.content, range);
        let new_text = single_line(new_text);
        self.content = SharedString::from(
            self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..],
        );
        let caret = range.start + new_text.len();
        self.selected = caret..caret;
        self.reversed = false;
        self.marked = None;
        tracing::trace!(len = self.content.len(), "text inserted");
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range| self.range_from_utf16(range))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let range = clamp_range(&self.content, range);
        self.content = SharedString::from(
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..],
        );
        // The composition stays marked, so it is drawn underlined until the IME
        // commits it through `replace_text_in_range`.
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        self.selected = new_selected_range_utf16
            .as_ref()
            .map(|selection| self.range_from_utf16(selection))
            .map(|selection| selection.start + range.start..selection.end + range.start)
            .unwrap_or_else(|| {
                let caret = range.start + new_text.len();
                caret..caret
            });
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        let (start, end) = if self.masked {
            (
                masked_offset(&self.content, range.start),
                masked_offset(&self.content, range.end),
            )
        } else {
            (range.start, range.end)
        };
        // The candidate window opens here, so the geometry has to be real, and it
        // moves with the text when the field is scrolled.
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(start) - self.scroll,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(end) - self.scroll,
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let line = self.last_layout.as_ref()?;
        let index = line.index_for_x(point.x - local.x + self.scroll)?;
        let content_index = if self.masked {
            unmasked_offset(&self.content, index)
        } else {
            index
        };
        Some(self.offset_to_utf16(content_index))
    }
}

/// The shaped text the field paints.
struct FieldElement {
    input: Entity<Draft>,
}

struct Prepaint {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    offset: Pixels,
}

impl IntoElement for FieldElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl gpui::Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let input = self.input.read(cx);
        let content = input.content.clone();
        let masked = input.masked && !content.is_empty();
        let look = input.look;
        let selected = if masked {
            masked_offset(&content, input.selected.start)
                ..masked_offset(&content, input.selected.end)
        } else {
            input.selected.clone()
        };
        let cursor = if masked {
            masked_offset(&content, input.cursor())
        } else {
            input.cursor()
        };
        let marked = input.marked.as_ref().map(|range| {
            if masked {
                masked_offset(&content, range.start)..masked_offset(&content, range.end)
            } else {
                range.clone()
            }
        });
        let style = window.text_style();
        let (text, color) = if content.is_empty() {
            (input.placeholder.clone(), look.placeholder)
        } else if masked {
            let bullets: String = std::iter::repeat_n(MASK_CHAR, content.chars().count()).collect();
            (SharedString::from(bullets), look.text)
        } else {
            (content, look.text)
        };
        let run = TextRun {
            len: text.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match marked.as_ref() {
            Some(marked) => vec![
                TextRun {
                    len: marked.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: text.len() - marked.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect(),
            None => vec![run],
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(text, font_size, &runs, None);
        // Keep the caret inside the field. The shape is one line and the field
        // clips it, so without this a long value (a pasted key, a URL) would run
        // past the border and over whatever sits next to it.
        let caret = line.x_for_index(cursor);
        let offset = (caret - bounds.size.width + px(4.)).max(px(0.));
        let caret_shift = if cursor == 0 { px(3.) } else { px(1.) };
        let (selection, cursor) = if selected.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + caret - offset - caret_shift, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    look.caret,
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected.start) - offset,
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected.end) - offset,
                            bounds.bottom(),
                        ),
                    ),
                    look.selection,
                )),
                None,
            )
        };
        Prepaint {
            line: Some(line),
            cursor,
            selection,
            offset,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let offset = prepaint.offset;
        let line = prepaint.line.take().expect("the line was shaped");
        // The mask is what keeps a long value inside the border; the offset is
        // what keeps its end (and the caret) inside the same box.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            line.paint(
                bounds.origin - point(offset, px(0.)),
                window.line_height(),
                window,
                cx,
            )
            .expect("the line paints");
        });
        if focus.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.scroll = offset;
        });
    }
}

impl Render for Draft {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus.is_focused(window);
        let look = self.look;
        div()
            .id("field")
            .key_context(self.context)
            .track_focus(&self.focus)
            .cursor(CursorStyle::IBeam)
            .flex()
            .items_center()
            .w_full()
            .when(look.borderless, |this| this.px(px(4.)).py_1())
            .when(!look.borderless, |this| {
                this.px_3()
                    .py(px(7.))
                    .bg(look.bg)
                    .when(look.pill, |this| this.rounded_full())
                    .when(!look.pill, |this| this.rounded_xl())
                    .border_2()
                    .border_color(if focused {
                        look.border_focused
                    } else {
                        look.border
                    })
            })
            .text_color(look.text)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(
                div()
                    .w_full()
                    .pl(px(4.))
                    .child(FieldElement { input: cx.entity() }),
            )
    }
}

impl Focusable for Draft {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Registers the field's key bindings. Called once at startup.
///
/// `secondary` is Command on macOS and Control elsewhere, which is the
/// shortcut users actually press. `cmd` is the Windows/Super key on Windows.
pub fn bind(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("backspace", Backspace, None),
        gpui::KeyBinding::new("delete", Delete, None),
        gpui::KeyBinding::new("left", Left, None),
        gpui::KeyBinding::new("right", Right, None),
        gpui::KeyBinding::new("shift-left", SelectLeft, None),
        gpui::KeyBinding::new("shift-right", SelectRight, None),
        gpui::KeyBinding::new("secondary-a", SelectAll, Some("ChatInput")),
        gpui::KeyBinding::new("secondary-a", SelectAll, Some("TextField")),
        gpui::KeyBinding::new("secondary-v", Paste, Some("ChatInput")),
        gpui::KeyBinding::new("secondary-v", Paste, Some("TextField")),
        gpui::KeyBinding::new("secondary-c", Copy, Some("ChatInput")),
        gpui::KeyBinding::new("secondary-c", Copy, Some("TextField")),
        gpui::KeyBinding::new("secondary-x", Cut, Some("ChatInput")),
        gpui::KeyBinding::new("secondary-x", Cut, Some("TextField")),
        gpui::KeyBinding::new("home", Home, None),
        gpui::KeyBinding::new("end", End, None),
    ]);
}

#[cfg(test)]
mod tests {
    use super::clamp_range;

    #[test]
    fn a_stale_mark_cannot_panic() {
        // The composer was cleared while a composition was open: the IME's
        // range still names byte 44 of a string that is now empty.
        assert_eq!(clamp_range("", 44..44), 0..0);
        assert_eq!(clamp_range("", 0..7), 0..0);
        assert_eq!(clamp_range("hi", 0..44), 0..2);
    }

    #[test]
    fn a_range_lands_on_character_boundaries() {
        // "あ" is three bytes; a range that starts inside it moves back to 0.
        assert_eq!(clamp_range("あい", 1..2), 0..0);
        assert_eq!(clamp_range("あい", 0..1), 0..0);
        assert_eq!(clamp_range("あい", 3..6), 3..6);
        assert_eq!(clamp_range("ab", 1..2), 1..2);
    }
    #[test]
    fn a_point_cannot_name_a_position_past_the_text() {
        assert_eq!(super::clamp_index("", 7), 0);
        assert_eq!(super::clamp_index("hi", 99), 2);
        assert_eq!(super::clamp_index("あ", 1), 0);
        assert_eq!(super::clamp_index("あ", 3), 3);
    }
    #[test]
    fn clipboard_shortcuts_use_the_platform_modifier() {
        // `secondary` is Command on macOS and Control elsewhere. `cmd` parses to
        // the platform/Super modifier, which is the Windows key on Windows, so a
        // paste bound to it never fires from Control-V.
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
        let bullet_len = super::MASK_CHAR.len_utf8();
        assert_eq!(super::masked_offset(text, 0), 0);
        assert_eq!(super::masked_offset(text, 1), bullet_len);
        assert_eq!(super::masked_offset(text, 4), 2 * bullet_len);
        assert_eq!(super::masked_offset(text, 5), 3 * bullet_len);
        assert_eq!(super::masked_offset(text, text.len()), 4 * bullet_len);
        // An interior byte clamps down safely rather than panicking.
        assert_eq!(super::masked_offset(text, 2), bullet_len);

        assert_eq!(super::unmasked_offset(text, 0), 0);
        assert_eq!(super::unmasked_offset(text, bullet_len), 1);
        assert_eq!(super::unmasked_offset(text, 2 * bullet_len), 4);
        assert_eq!(super::unmasked_offset(text, 3 * bullet_len), 5);
        assert_eq!(super::unmasked_offset(text, 4 * bullet_len), text.len());
        assert_eq!(super::unmasked_offset(text, 99), text.len());
    }
}
