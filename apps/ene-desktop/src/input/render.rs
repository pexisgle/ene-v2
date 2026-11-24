use super::Draft;
use super::prepaint::{
    Prepaint, prepaint_multiline, prepaint_single_line, prepare_display_text, shape_wrapped,
};
use super::selection::masked_offset;
use gpui::{
    App, Bounds, ContentMask, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, MouseButton, Pixels, Point, Render,
    ShapedLine, Style, Window, WrappedLine, div, point, prelude::*, px, relative, size,
};
use std::ops::Range;

/// How many lines the chat field grows to before it scrolls instead.
const MAX_INPUT_ROWS: f32 = 5.;

pub(super) enum FieldLayout {
    Single(Box<ShapedLine>),
    Multiline(Vec<WrappedLine>, Pixels),
}

impl Draft {
    pub(super) fn compute_bounds_for_range(
        &self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
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
        let (start, end, height) = match line {
            FieldLayout::Single(line) => (
                point(line.x_for_index(start), px(0.)),
                point(line.x_for_index(end), px(0.)),
                bounds.size.height,
            ),
            FieldLayout::Multiline(lines, height) => (
                wrapped_position(lines, *height, start)?,
                wrapped_position(lines, *height, end)?,
                *height,
            ),
        };
        let origin = bounds.origin - point(self.scroll, px(0.));
        let width = if start.y == end.y {
            (end.x - start.x).max(px(2.))
        } else {
            px(2.)
        };
        Some(Bounds::new(origin + start, size(width, height)))
    }
}

pub(super) fn wrapped_index(
    lines: &[WrappedLine],
    height: Pixels,
    mut position: Point<Pixels>,
) -> usize {
    if position.y < px(0.) {
        return 0;
    }
    let mut start = 0;
    for line in lines {
        let line_height = line.size(height).height;
        if position.y < line_height {
            let index = line.closest_index_for_position(position, height);
            return start + index.unwrap_or_else(|boundary| boundary);
        }
        position.y -= line_height;
        start += line.len() + 1;
    }
    start.saturating_sub(1)
}

pub(super) fn wrapped_position(
    lines: &[WrappedLine],
    height: Pixels,
    mut index: usize,
) -> Option<Point<Pixels>> {
    let mut top = px(0.);
    for line in lines {
        if index <= line.len() {
            if let Some(row) = line
                .wrap_boundaries()
                .iter()
                .position(|wrap| line.runs()[wrap.run_ix].glyphs[wrap.glyph_ix].index == index)
            {
                return Some(point(px(0.), top + height * (row + 1)));
            }
            return line
                .position_for_index(index, height)
                .map(|point| point + gpui::point(px(0.), top));
        }
        index -= line.len() + 1;
        top += line.size(height).height;
    }
    None
}

struct FieldElement {
    input: Entity<Draft>,
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
        if self.input.read(cx).multiline() {
            let input = self.input.clone();
            let text_style = window.text_style();
            let font_size = text_style.font_size.to_pixels(window.rem_size());
            let line_height = window.line_height();
            return (
                window.request_measured_layout(style, move |known, space, window, cx| {
                    let input = input.read(cx);
                    let text = if input.content.is_empty() {
                        input.placeholder.clone()
                    } else {
                        input.content.clone()
                    };
                    let width = known.width.or(match space.width {
                        gpui::AvailableSpace::Definite(width) => Some(width),
                        _ => None,
                    });
                    let run = text_style.to_run(text.len());
                    let lines = shape_wrapped(window, text, font_size, &[run], width);
                    let height = lines.iter().fold(px(0.), |height, line| {
                        height + line.size(line_height).height
                    });
                    let content_width = lines
                        .iter()
                        .map(|line| line.width())
                        .max()
                        .unwrap_or_default();
                    // One line when empty, growing with the text up to five.
                    // Past that the field scrolls, so a long draft cannot push
                    // the pod off the screen.
                    let desired = height.max(line_height);
                    size(
                        width.unwrap_or(content_width),
                        desired.min(line_height * MAX_INPUT_ROWS),
                    )
                }),
                (),
            );
        }
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
        let prepared = prepare_display_text(self.input.read(cx), &window.text_style());
        if prepared.multiline {
            prepaint_multiline(prepared, bounds, window)
        } else {
            prepaint_single_line(prepared, bounds, window)
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
        let Some(layout) = prepaint.layout.take() else {
            return;
        };
        window.with_content_mask(Some(ContentMask { bounds }), |window| match &layout {
            FieldLayout::Single(line) => {
                let _painted = line.paint(
                    bounds.origin - point(offset, px(0.)),
                    window.line_height(),
                    window,
                    cx,
                );
            }
            FieldLayout::Multiline(lines, height) => {
                let mut origin = bounds.origin;
                for line in lines {
                    let _painter = line.paint_background(
                        origin,
                        *height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    let _painted =
                        line.paint(origin, *height, gpui::TextAlign::Left, None, window, cx);
                    origin.y += line.size(*height).height;
                }
            }
        });
        if focus.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(layout);
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
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
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
