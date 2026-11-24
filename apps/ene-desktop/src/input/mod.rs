//! One editable text field, built on the input handler GPUI ships.
//!
//! GPUI 0.2.2 has no text field of its own: an editor implements
//! `EntityInputHandler`, paints a `TextElement` and calls `Window::handle_input`
//! during paint. This module is that implementation, adapted from the crate's
//! `examples/input.rs`, and it is the only reason composition (Japanese IME)
//! works.

mod prepaint;
mod render;
mod selection;
#[cfg(test)]
mod tests;

use gpui::{
    App, Bounds, Context, EntityInputHandler, FocusHandle, Focusable, Hsla, Pixels, SharedString,
    UTF16Selection, Window, rgb, rgba,
};
use selection::{clamp_range, offset_from_utf16_in, single_line};
use std::ops::Range;

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
        Newline,
        Up,
        Down,
        SelectUp,
        SelectDown,
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
    context: &'static str,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    last_layout: Option<render::FieldLayout>,
    last_bounds: Option<Bounds<Pixels>>,
    scroll: Pixels,
    selecting: bool,
    masked: bool,
    look: FieldLook,
}

impl Draft {
    fn multiline(&self) -> bool {
        self.context == "TextField Multiline"
    }

    pub fn new(
        cx: &mut Context<Self>,
        placeholder: impl Into<SharedString>,
        context: &'static str,
    ) -> Self {
        let placeholder: SharedString = placeholder.into();
        let placeholder = SharedString::from(single_line(&placeholder));
        Self {
            focus: cx.focus_handle(),
            content: SharedString::default(),
            placeholder,
            context,
            selected: 0..0,
            reversed: false,
            marked: None,
            last_layout: None,
            last_bounds: None,
            scroll: gpui::px(0.),
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
        let placeholder: SharedString = placeholder.into();
        self.placeholder = SharedString::from(single_line(&placeholder));
        cx.notify();
    }

    /// The text as it stands, including an open composition.
    pub fn text(&self) -> &str {
        &self.content
    }

    /// Whether an IME composition is open.
    pub fn composing(&self) -> bool {
        self.marked.is_some()
    }

    /// The field's focus handle, for focusing it when a panel opens.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Replaces the text and puts the caret at the end.
    pub fn set(&mut self, text: impl Into<SharedString>, cx: &mut Context<Self>) {
        let raw: SharedString = text.into();
        self.content = self.normalize(&raw).into();
        let end = self.content.len();
        self.selected = end..end;
        self.reversed = false;
        self.marked = None;
        cx.notify();
    }

    /// Empties the field.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.set("", cx);
    }

    pub(super) fn normalize(&self, text: &str) -> String {
        if self.multiline() {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            single_line(text)
        }
    }
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
        let new_text = self.normalize(new_text);
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
        let new_text = self.normalize(new_text);
        self.content = SharedString::from(
            self.content[0..range.start].to_owned() + &new_text + &self.content[range.end..],
        );
        self.marked = (!new_text.is_empty()).then(|| range.start..range.start + new_text.len());
        let selected = new_selected_range_utf16.as_ref().map_or_else(
            || {
                let caret = range.start + new_text.len();
                caret..caret
            },
            |selection| {
                let start = offset_from_utf16_in(&new_text, selection.start);
                let end = offset_from_utf16_in(&new_text, selection.end);
                range.start + start..range.start + end
            },
        );
        self.selected = clamp_range(&self.content, selected);
        self.reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.compute_bounds_for_range(range_utf16, bounds)
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.last_bounds?.localize(&point)?;
        Some(self.offset_to_utf16(self.index_for_mouse_position(point)))
    }
}

impl Focusable for Draft {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// Registers the field's key bindings. Called once at startup.
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
        gpui::KeyBinding::new("enter", Newline, Some("Multiline")),
        gpui::KeyBinding::new("up", Up, Some("Multiline")),
        gpui::KeyBinding::new("down", Down, Some("Multiline")),
        gpui::KeyBinding::new("shift-up", SelectUp, Some("Multiline")),
        gpui::KeyBinding::new("shift-down", SelectDown, Some("Multiline")),
    ]);
}
