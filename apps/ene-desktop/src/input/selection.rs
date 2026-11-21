use super::render::{FieldLayout, wrapped_index, wrapped_position};
use super::{
    Copy, Cut, Down, Draft, End, Home, Left, Newline, Paste, Right, SelectAll, SelectDown,
    SelectLeft, SelectRight, SelectUp, Up,
};
use gpui::{
    ClipboardItem, Context, EntityInputHandler, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, Window, point, px,
};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const MASK_CHAR: char = '•';

impl Draft {
    pub(super) fn offset_from_utf16(&self, offset: usize) -> usize {
        offset_from_utf16_in(&self.content, offset)
    }

    pub(super) fn offset_to_utf16(&self, offset: usize) -> usize {
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

    pub(super) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(super) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    pub(super) fn cursor(&self) -> usize {
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

    pub(super) fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let previous = self.previous_boundary(self.cursor());
            self.move_to(previous, cx);
        } else {
            let start = self.selected.start;
            self.move_to(start, cx);
        }
    }

    pub(super) fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.move_to(next, cx);
        } else {
            let end = self.selected.end;
            self.move_to(end, cx);
        }
    }

    pub(super) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        let previous = self.previous_boundary(self.cursor());
        self.select_to(previous, cx);
    }

    pub(super) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_boundary(self.cursor());
        self.select_to(next, cx);
    }

    pub(super) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        let end = self.content.len();
        self.select_to(end, cx);
    }

    pub(super) fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.index_in_visual_row(0., Some(px(0.))).unwrap_or(0);
        self.move_to(offset, cx);
    }

    pub(super) fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let end = self
            .index_in_visual_row(0., Some(px(f32::MAX)))
            .unwrap_or(self.content.len());
        self.move_to(end, cx);
    }

    pub(super) fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if !self.composing() {
            self.replace_text_in_range(None, "\n", window, cx);
        }
    }

    fn index_in_visual_row(&self, dy: f32, x: Option<Pixels>) -> Option<usize> {
        let FieldLayout::Multiline(lines, height) = self.last_layout.as_ref()? else {
            return None;
        };
        if self.content.is_empty() {
            return Some(0);
        }
        let mut position = wrapped_position(lines, *height, self.cursor())?;
        position.x = x.unwrap_or(position.x);
        position.y += dy * *height + *height / 2.;
        Some(clamp_index(
            &self.content,
            wrapped_index(lines, *height, position),
        ))
    }

    pub(super) fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.index_in_visual_row(-1., None) {
            self.move_to(index, cx);
        }
    }

    pub(super) fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.index_in_visual_row(1., None) {
            self.move_to(index, cx);
        }
    }

    pub(super) fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.index_in_visual_row(-1., None) {
            self.select_to(index, cx);
        }
    }

    pub(super) fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.index_in_visual_row(1., None) {
            self.select_to(index, cx);
        }
    }

    pub(super) fn backspace(
        &mut self,
        _: &super::Backspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_empty() {
            let previous = self.previous_boundary(self.cursor());
            self.select_to(previous, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub(super) fn delete(
        &mut self,
        _: &super::Delete,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    pub(super) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub(super) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.masked && !self.selected.is_empty() {
            let text = self.content[self.selected.clone()].to_owned();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus);
        self.selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }

    pub(super) fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting {
            let offset = self.index_for_mouse_position(event.position);
            self.select_to(offset, cx);
        }
    }

    pub(super) fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
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
        if self.content.is_empty() {
            return 0;
        }
        let local = position - bounds.origin + point(self.scroll, px(0.));
        let shaped_index = match line {
            FieldLayout::Single(line) => line.closest_index_for_x(local.x),
            FieldLayout::Multiline(lines, height) => wrapped_index(lines, *height, local),
        };
        if self.masked {
            unmasked_offset(&self.content, shaped_index)
        } else {
            clamp_index(&self.content, shaped_index)
        }
    }
}

/// Converts a UTF-8 byte offset in `content` to the byte offset in its masked representation.
pub(super) fn masked_offset(content: &str, byte_idx: usize) -> usize {
    let clamped = clamp_index(content, byte_idx);
    content[..clamped].chars().count() * MASK_CHAR.len_utf8()
}

/// Converts a UTF-8 byte offset in the bullet-masked string back to `content`.
pub(super) fn unmasked_offset(content: &str, masked_byte_idx: usize) -> usize {
    let char_index = masked_byte_idx / MASK_CHAR.len_utf8();
    content
        .char_indices()
        .nth(char_index)
        .map_or(content.len(), |(byte_idx, _)| byte_idx)
}

/// Converts a UTF-16 code-unit offset inside `text` to a UTF-8 byte offset.
pub(super) fn offset_from_utf16_in(text: &str, offset: usize) -> usize {
    let mut utf8 = 0;
    let mut utf16 = 0;
    for character in text.chars() {
        if utf16 >= offset {
            break;
        }
        utf16 += character.len_utf16();
        utf8 += character.len_utf8();
    }
    utf8
}

/// Collapses a paste into one line.
pub(super) fn single_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// A byte index inside `text`, on a character boundary.
pub(super) fn clamp_index(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// A byte range clamped into `text` on character boundaries.
pub(super) fn clamp_range(text: &str, range: Range<usize>) -> Range<usize> {
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
