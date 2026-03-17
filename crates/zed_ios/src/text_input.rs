//! Simple text input component for iOS.
//!
//! A minimal text input implementation suitable for connection forms.
//! This is a simplified version that uses GPUI's built-in text handling.

use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId, Hsla, IntoElement,
    LayoutId, MouseButton, MouseDownEvent, PaintQuad, Pixels, Point, Render, ShapedLine,
    SharedString, Style, TextRun, UTF16Selection, Window, div, fill, point, prelude::*, px,
    relative, size,
};
use theme::ActiveTheme;
use unicode_segmentation::UnicodeSegmentation;

gpui::actions!(
    text_input,
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

/// A simple single-line text input component.
pub struct TextInput {
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    is_masked: bool,
    label: Option<SharedString>,
}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            is_masked: false,
            label: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[allow(dead_code)]
    pub fn with_masked(mut self, masked: bool) -> Self {
        self.is_masked = masked;
        self
    }

    /// Alias for with_masked - makes the text input show dots instead of characters
    pub fn with_secure(self, secure: bool) -> Self {
        self.with_masked(secure)
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content.clear();
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.last_layout = None;
        self.last_bounds = None;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.select_to(prev, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            self.select_to(next, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        window.focus(&self.focus_handle, cx);

        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text.replace('\n', " "), window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }

        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }

        let Some(layout) = self.last_layout.as_ref() else {
            return self.content.len();
        };
        let Some(bounds) = self.last_bounds else {
            return self.content.len();
        };

        if position.x <= bounds.origin.x {
            return 0;
        }

        if position.x >= bounds.origin.x + bounds.size.width {
            return self.content.len();
        }

        let position = position - bounds.origin;
        let display_index = layout.closest_index_for_x(position.x);
        let display_text = self.display_text();
        self.content_offset_for_display_index(display_index, &display_text)
    }

    fn display_index_for_content_offset(&self, content_offset: usize, display_text: &str) -> usize {
        let grapheme_ord = self
            .content
            .grapheme_indices(true)
            .position(|(idx, _)| idx >= content_offset)
            .unwrap_or_else(|| self.content.graphemes(true).count());

        display_text
            .grapheme_indices(true)
            .nth(grapheme_ord)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| display_text.len())
    }

    fn content_offset_for_display_index(&self, display_index: usize, display_text: &str) -> usize {
        let grapheme_ord = display_text
            .grapheme_indices(true)
            .position(|(idx, _)| idx >= display_index)
            .unwrap_or_else(|| display_text.graphemes(true).count());

        self.content
            .grapheme_indices(true)
            .nth(grapheme_ord)
            .map(|(idx, _)| idx)
            .unwrap_or_else(|| self.content.len())
    }

    fn display_range_for_content_range(
        &self,
        content_range: Range<usize>,
        display_text: &str,
    ) -> Range<usize> {
        let start = self.display_index_for_content_offset(content_range.start, display_text);
        let end = self.display_index_for_content_offset(content_range.end, display_text);
        start..end
    }

    fn content_range_for_display_utf16_range(
        &self,
        range_utf16: Range<usize>,
        display_text: &str,
    ) -> Option<Range<usize>> {
        let display_range = range_from_utf16(display_text, range_utf16)?;
        let start = self.content_offset_for_display_index(display_range.start, display_text);
        let end = self.content_offset_for_display_index(display_range.end, display_text);
        Some(start..end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }

    fn display_text(&self) -> String {
        if self.is_masked && !self.content.is_empty() {
            "\u{2022}".repeat(self.content.graphemes(true).count())
        } else {
            self.content.clone()
        }
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let content = self.display_text();
        let range = range_from_utf16(&content, range_utf16)?;
        let adjusted = range_to_utf16(&content, range.clone());
        *_adjusted_range = Some(adjusted);
        Some(content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let display_text = self.display_text();
        let display_range =
            self.display_range_for_content_range(self.selected_range.clone(), &display_text);
        Some(UTF16Selection {
            range: range_to_utf16(&display_text, display_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        let display_text = self.display_text();
        self.marked_range.as_ref().map(|range| {
            let display_range = self.display_range_for_content_range(range.clone(), &display_text);
            range_to_utf16(&display_text, display_range)
        })
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let display_text = self.display_text();
        let range = range_utf16
            .and_then(|range_utf16| {
                self.content_range_for_display_utf16_range(range_utf16, &display_text)
            })
            .or_else(|| self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content = self.content[0..range.start].to_string()
            + new_text
            + &self.content[range.end..self.content.len()];

        let new_position = range.start + new_text.len();
        self.selected_range = new_position..new_position;
        self.selection_reversed = false;
        self.marked_range.take();
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
        let display_text = self.display_text();
        let range = range_utf16
            .and_then(|range_utf16| {
                self.content_range_for_display_utf16_range(range_utf16, &display_text)
            })
            .or_else(|| self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content = self.content[0..range.start].to_string()
            + new_text
            + &self.content[range.end..self.content.len()];

        let new_selected = new_selected_range_utf16
            .and_then(|range_utf16| range_from_utf16(new_text, range_utf16))
            .map(|new_range| {
                let start = range.start + new_range.start;
                let end = range.start + new_range.end;
                start..end
            })
            .unwrap_or_else(|| {
                let new_end = range.start + new_text.len();
                new_end..new_end
            });

        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected;
        self.selection_reversed = false;
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let Some(layout) = self.last_layout.as_ref() else {
            return None;
        };
        let display_text = self.display_text();
        let range = range_from_utf16(&display_text, range_utf16)?;
        Some(Bounds {
            origin: Point {
                x: element_bounds.origin.x + layout.x_for_index(range.start),
                y: element_bounds.origin.y,
            },
            size: size(
                layout.x_for_index(range.end) - layout.x_for_index(range.start),
                element_bounds.size.height,
            ),
        })
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let Some(layout) = self.last_layout.as_ref() else {
            return None;
        };
        let Some(bounds) = self.last_bounds else {
            return None;
        };
        let point = point - bounds.origin;
        let index = layout.closest_index_for_x(point.x);
        let display_text = self.display_text();
        let utf16_index = range_to_utf16(&display_text, 0..index).end;
        Some(utf16_index)
    }
}

fn range_to_utf16(s: &str, range: Range<usize>) -> Range<usize> {
    let start = s[0..range.start].encode_utf16().count();
    let end = start + s[range].encode_utf16().count();
    start..end
}

fn range_from_utf16(s: &str, range_utf16: Range<usize>) -> Option<Range<usize>> {
    let mut utf8_offset = 0;
    let mut utf16_offset = 0;
    let mut start = None;
    let mut end = None;

    for ch in s.chars() {
        if utf16_offset == range_utf16.start {
            start = Some(utf8_offset);
        }
        if utf16_offset == range_utf16.end {
            end = Some(utf8_offset);
            break;
        }
        utf8_offset += ch.len_utf8();
        utf16_offset += ch.len_utf16();
    }

    if utf16_offset == range_utf16.end {
        end = Some(utf8_offset);
    }

    Some(start?..end?)
}

impl Focusable for TextInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_focused = self.focus_handle.is_focused(window);

        let colors = cx.theme().colors();
        let bg_color = colors.element_background;
        let border_color = if is_focused {
            colors.border_focused
        } else {
            colors.border
        };
        let text_color = colors.text;
        let placeholder_color = colors.text_placeholder;
        let selection_color = colors.element_selection_background;

        let display_text = self.display_text();
        let show_placeholder = display_text.is_empty();

        let current_text_color = if show_placeholder {
            placeholder_color
        } else {
            text_color
        };

        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .w_full()
            .when_some(self.label.clone(), |el, label| {
                el.child(
                    div()
                        .text_size(px(14.0))
                        .text_color(colors.text_muted)
                        .child(label),
                )
            })
            .child(
                div()
                    .id("text-input-wrapper")
                    .key_context("TextInput")
                    .track_focus(&self.focus_handle)
                    .cursor(CursorStyle::IBeam)
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
                    .bg(bg_color)
                    .border_1()
                    .border_color(border_color)
                    .rounded(px(8.0))
                    .px(px(12.0))
                    .py(px(10.0))
                    .min_h(px(44.0))
                    .overflow_hidden()
                    .child(TextLineElement {
                        input: cx.entity(),
                        text_color: current_text_color,
                        placeholder_color,
                        selection_color,
                        caret_color: text_color,
                        font_size: px(16.0),
                        show_placeholder,
                    })
                    .when(is_focused, |el| {
                        el.on_key_down(cx.listener(
                            |this, event: &gpui::KeyDownEvent, window, cx| {
                                let key: &str = event.keystroke.key.as_ref();

                                match key {
                                    "backspace" => this.backspace(&Backspace, window, cx),
                                    "delete" => this.delete(&Delete, window, cx),
                                    "left" => this.left(&Left, window, cx),
                                    "right" => this.right(&Right, window, cx),
                                    "home" => this.home(&Home, window, cx),
                                    "end" => this.end(&End, window, cx),
                                    "enter" => {
                                        // Enter is handled by the connect view, not here.
                                        // Let it propagate.
                                    }
                                    _ => {}
                                }
                            },
                        ))
                    }),
            )
    }
}

struct TextLineElement {
    input: Entity<TextInput>,
    text_color: Hsla,
    placeholder_color: Hsla,
    selection_color: Hsla,
    caret_color: Hsla,
    font_size: Pixels,
    show_placeholder: bool,
}

struct TextLinePrepaint {
    line: Option<ShapedLine>,
    selection: Option<PaintQuad>,
    cursor: Option<PaintQuad>,
}

impl IntoElement for TextLineElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextLineElement {
    type RequestLayoutState = ();
    type PrepaintState = TextLinePrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], _cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let content = input.display_text();
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let is_placeholder = self.show_placeholder;

        let display_text = if is_placeholder {
            input.placeholder.to_string()
        } else {
            content.clone()
        };

        let style = window.text_style();
        let run_color = if is_placeholder {
            self.placeholder_color
        } else {
            self.text_color
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: run_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let font_size = self.font_size;
        let line =
            window
                .text_system()
                .shape_line(display_text.clone().into(), font_size, &[run], None);

        let selection = if !selected_range.is_empty() {
            let display_range =
                input.display_range_for_content_range(selected_range, &display_text);
            Some(fill(
                Bounds::from_corners(
                    point(
                        bounds.origin.x + line.x_for_index(display_range.start),
                        bounds.origin.y,
                    ),
                    point(
                        bounds.origin.x + line.x_for_index(display_range.end),
                        bounds.origin.y + bounds.size.height,
                    ),
                ),
                self.selection_color,
            ))
        } else {
            None
        };

        let cursor_quad = if is_placeholder {
            None
        } else {
            let display_cursor = input.display_index_for_content_offset(cursor, &display_text);
            Some(fill(
                Bounds::new(
                    point(
                        bounds.origin.x + line.x_for_index(display_cursor),
                        bounds.origin.y,
                    ),
                    size(px(2.0), bounds.size.height),
                ),
                self.caret_color,
            ))
        };

        TextLinePrepaint {
            line: Some(line),
            selection,
            cursor: cursor_quad,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }

        let Some(line) = prepaint.line.take() else {
            return;
        };

        window.handle_input(
            &self.input.read(cx).focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        let _ = line.paint(
            bounds.origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        );

        if self.input.read(cx).focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}
