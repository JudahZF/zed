//! Text input helpers for iOS.
//!
//! This module provides utilities for handling text input on iOS, including
//! key code mappings and UITextInput protocol helpers.

use crate::Keystroke;

/// Maps iOS key codes to GPUI key names.
///
/// iOS key codes come from UIKeyboardHIDUsage values when using hardware keyboards.
pub fn key_code_to_key_name(key_code: i64) -> Option<&'static str> {
    // UIKeyboardHIDUsage values (from USB HID spec)
    match key_code {
        // Letters (0x04 - 0x1D)
        0x04 => Some("a"),
        0x05 => Some("b"),
        0x06 => Some("c"),
        0x07 => Some("d"),
        0x08 => Some("e"),
        0x09 => Some("f"),
        0x0A => Some("g"),
        0x0B => Some("h"),
        0x0C => Some("i"),
        0x0D => Some("j"),
        0x0E => Some("k"),
        0x0F => Some("l"),
        0x10 => Some("m"),
        0x11 => Some("n"),
        0x12 => Some("o"),
        0x13 => Some("p"),
        0x14 => Some("q"),
        0x15 => Some("r"),
        0x16 => Some("s"),
        0x17 => Some("t"),
        0x18 => Some("u"),
        0x19 => Some("v"),
        0x1A => Some("w"),
        0x1B => Some("x"),
        0x1C => Some("y"),
        0x1D => Some("z"),

        // Numbers (0x1E - 0x27)
        0x1E => Some("1"),
        0x1F => Some("2"),
        0x20 => Some("3"),
        0x21 => Some("4"),
        0x22 => Some("5"),
        0x23 => Some("6"),
        0x24 => Some("7"),
        0x25 => Some("8"),
        0x26 => Some("9"),
        0x27 => Some("0"),

        // Special keys
        0x28 => Some("enter"),
        0x29 => Some("escape"),
        0x2A => Some("backspace"),
        0x2B => Some("tab"),
        0x2C => Some("space"),
        0x2D => Some("-"),
        0x2E => Some("="),
        0x2F => Some("["),
        0x30 => Some("]"),
        0x31 => Some("\\"),
        0x33 => Some(";"),
        0x34 => Some("'"),
        0x35 => Some("`"),
        0x36 => Some(","),
        0x37 => Some("."),
        0x38 => Some("/"),
        0x39 => Some("capslock"),

        // Function keys (0x3A - 0x45)
        0x3A => Some("f1"),
        0x3B => Some("f2"),
        0x3C => Some("f3"),
        0x3D => Some("f4"),
        0x3E => Some("f5"),
        0x3F => Some("f6"),
        0x40 => Some("f7"),
        0x41 => Some("f8"),
        0x42 => Some("f9"),
        0x43 => Some("f10"),
        0x44 => Some("f11"),
        0x45 => Some("f12"),

        // Navigation
        0x46 => Some("printscreen"),
        0x47 => Some("scrolllock"),
        0x48 => Some("pause"),
        0x49 => Some("insert"),
        0x4A => Some("home"),
        0x4B => Some("pageup"),
        0x4C => Some("delete"),
        0x4D => Some("end"),
        0x4E => Some("pagedown"),
        0x4F => Some("right"),
        0x50 => Some("left"),
        0x51 => Some("down"),
        0x52 => Some("up"),

        // Numpad
        0x53 => Some("numlock"),
        0x54 => Some("numpad_divide"),
        0x55 => Some("numpad_multiply"),
        0x56 => Some("numpad_subtract"),
        0x57 => Some("numpad_add"),
        0x58 => Some("numpad_enter"),
        0x59 => Some("numpad1"),
        0x5A => Some("numpad2"),
        0x5B => Some("numpad3"),
        0x5C => Some("numpad4"),
        0x5D => Some("numpad5"),
        0x5E => Some("numpad6"),
        0x5F => Some("numpad7"),
        0x60 => Some("numpad8"),
        0x61 => Some("numpad9"),
        0x62 => Some("numpad0"),
        0x63 => Some("numpad_decimal"),

        // Modifiers (for reference, usually handled separately)
        0xE0 => Some("control"),
        0xE1 => Some("shift"),
        0xE2 => Some("alt"),
        0xE3 => Some("cmd"), // Left GUI/Command
        0xE4 => Some("control"),
        0xE5 => Some("shift"),
        0xE6 => Some("alt"),
        0xE7 => Some("cmd"), // Right GUI/Command

        _ => None,
    }
}

/// Convert a UIKey characters string to a GPUI keystroke.
pub fn characters_to_keystroke(
    characters: &str,
    key_code: Option<i64>,
    modifiers: crate::Modifiers,
) -> Option<Keystroke> {
    // Try to get key name from key code first
    let key = if let Some(code) = key_code {
        if let Some(name) = key_code_to_key_name(code) {
            name.into()
        } else if !characters.is_empty() {
            characters.to_lowercase().into()
        } else {
            return None;
        }
    } else if !characters.is_empty() {
        characters.to_lowercase().into()
    } else {
        return None;
    };

    Some(Keystroke {
        key,
        modifiers,
        key_char: if characters.is_empty() {
            None
        } else {
            Some(characters.to_string())
        },
    })
}

/// UITextInput position representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPosition {
    pub offset: usize,
}

/// UITextInput range representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub start: TextPosition,
    pub end: TextPosition,
}

impl TextRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start: TextPosition { offset: start },
            end: TextPosition { offset: end },
        }
    }

    pub fn empty(offset: usize) -> Self {
        Self::new(offset, offset)
    }

    pub fn is_empty(&self) -> bool {
        self.start.offset == self.end.offset
    }

    pub fn len(&self) -> usize {
        self.end.offset.saturating_sub(self.start.offset)
    }

    pub fn to_std_range(&self) -> std::ops::Range<usize> {
        self.start.offset..self.end.offset
    }
}

impl From<std::ops::Range<usize>> for TextRange {
    fn from(range: std::ops::Range<usize>) -> Self {
        Self::new(range.start, range.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_code_mapping() {
        assert_eq!(key_code_to_key_name(0x04), Some("a"));
        assert_eq!(key_code_to_key_name(0x1E), Some("1"));
        assert_eq!(key_code_to_key_name(0x28), Some("enter"));
        assert_eq!(key_code_to_key_name(0x2A), Some("backspace"));
        assert_eq!(key_code_to_key_name(0x4F), Some("right"));
        assert_eq!(key_code_to_key_name(0xFF), None);
    }

    #[test]
    fn test_text_range() {
        let range = TextRange::new(5, 10);
        assert_eq!(range.len(), 5);
        assert!(!range.is_empty());

        let empty = TextRange::empty(5);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }
}
