//! Simple text input component for iOS.
//!
//! A minimal text input implementation suitable for connection forms.
//! This is a simplified version that uses GPUI's built-in text handling.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, rgb, rgba, size, App, Bounds, ClipboardItem, Context, CursorStyle,
    EntityInputHandler, FocusHandle, Focusable, IntoElement, MouseButton, MouseDownEvent, Pixels,
    Point, Render, ShapedLine, SharedString, UTF16Selection, Window,
};
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
        self.selected_range = self.content.len()..self.content.len();
        cx.notify();
    }

    #[allow(dead_code)]
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.content.clear();
        self.selected_range = 0..0;
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
        eprintln!(
            "[TextInput] backspace called, cursor at {}, content len {}",
            self.cursor_offset(),
            self.content.len()
        );
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            eprintln!(
                "[TextInput] backspace: selecting from {} to {}",
                prev,
                self.cursor_offset()
            );
            self.select_to(prev, cx)
        }
        eprintln!(
            "[TextInput] backspace: replacing range {:?} with empty string",
            self.selected_range
        );
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        eprintln!(
            "[TextInput] delete called, cursor at {}, content len {}",
            self.cursor_offset(),
            self.content.len()
        );
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            eprintln!(
                "[TextInput] delete: selecting from {} to {}",
                self.cursor_offset(),
                next
            );
            self.select_to(next, cx)
        }
        eprintln!(
            "[TextInput] delete: replacing range {:?} with empty string",
            self.selected_range
        );
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
        let Some(layout) = self.last_layout.as_ref() else {
            return 0;
        };
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let position = position - bounds.origin;
        layout.closest_index_for_x(position.x)
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
        Some(content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: range_to_utf16(&self.content, self.selected_range.clone()),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| range_to_utf16(&self.content, range.clone()))
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
        let range = range_utf16
            .and_then(|range_utf16| range_from_utf16(&self.content, range_utf16))
            .unwrap_or(self.selected_range.clone());

        self.content = self.content[0..range.start].to_string()
            + new_text
            + &self.content[range.end..self.content.len()];

        let new_position = range.start + new_text.len();
        self.selected_range = new_position..new_position;
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
        let range = range_utf16
            .and_then(|range_utf16| range_from_utf16(&self.content, range_utf16))
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

        self.marked_range = Some(range.start..range.start + new_text.len());
        self.selected_range = new_selected;
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
        let range = range_from_utf16(&self.content, range_utf16)?;
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
        let utf16_index = range_to_utf16(&self.content, 0..index).end;
        Some(utf16_index)
    }
}

fn range_to_utf16(s: &str, range: Range<usize>) -> Range<usize> {
    let start = s[0..range.start].encode_utf16().count();
    let end = start + s[range.clone()].encode_utf16().count();
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
        let _entity = cx.entity().clone();

        let bg_color = rgb(0x313244);
        let border_color = if is_focused {
            rgb(0x89b4fa)
        } else {
            rgb(0x45475a)
        };
        let text_color = rgb(0xcdd6f4);
        let placeholder_color = rgb(0x6c7086);
        let _selection_color = rgba(0x89b4fa40);

        let display_text = self.display_text();
        let show_placeholder = display_text.is_empty();
        let selected_range = self.selected_range.clone();

        let text_to_show: SharedString = if show_placeholder {
            self.placeholder.clone()
        } else {
            display_text.into()
        };

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
                        .text_color(rgb(0xa6adc8))
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .size_full()
                            .text_size(px(16.0))
                            .text_color(current_text_color)
                            .child(text_to_show)
                            .when(
                                is_focused && !show_placeholder && selected_range.is_empty(),
                                |el| {
                                    el.child(
                                        div().w(px(2.0)).h(px(20.0)).bg(text_color).ml(px(-1.0)),
                                    )
                                },
                            ),
                    )
                    .when(is_focused, |el| {
                        el.on_key_down(cx.listener(
                            |this, event: &gpui::KeyDownEvent, window, cx| {
                                let key: &str = event.keystroke.key.as_ref();

                                if key.is_empty() {
                                    return;
                                }

                                if key.starts_with("unknown-") {
                                    return;
                                }

                                let modifier_keys = [
                                    "shift",
                                    "control",
                                    "alt",
                                    "cmd",
                                    "capslock",
                                    "numlock",
                                    "scrolllock",
                                    "printscreen",
                                    "pause",
                                ];
                                if modifier_keys.contains(&key) {
                                    return;
                                }

                                if key.starts_with("f") && key.len() <= 3 {
                                    if let Ok(_) = key[1..].parse::<u8>() {
                                        return;
                                    }
                                }

                                if key == "backspace" {
                                    this.backspace(&Backspace, window, cx);
                                    return;
                                }

                                if key == "delete" {
                                    this.delete(&Delete, window, cx);
                                    return;
                                }

                                let nav_keys = [
                                    "left", "right", "up", "down", "home", "end", "pageup",
                                    "pagedown", "insert", "escape", "tab",
                                ];
                                if nav_keys.contains(&key) {
                                    return;
                                }

                                if let Some(key_char) = &event.keystroke.key_char {
                                    if key_char.chars().all(|c| c.is_control()) {
                                        return;
                                    }

                                    if !event.keystroke.modifiers.control
                                        && !event.keystroke.modifiers.alt
                                        && !event.keystroke.modifiers.platform
                                    {
                                        this.replace_text_in_range(None, key_char, window, cx);
                                    }
                                }
                            },
                        ))
                    }),
            )
    }
}
