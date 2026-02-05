//! iOS event handling - touch and keyboard input translation.
//!
//! This module translates UIKit touch events to GPUI mouse events and
//! handles hardware keyboard input via UIKey events.

use crate::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, ScrollWheelEvent,
    TouchPhase as GpuiTouchPhase, point, px,
};
use objc::{msg_send, runtime::Object, sel, sel_impl};
use std::sync::atomic::{AtomicBool, Ordering};

use super::CGPoint;

/// Flag to enable verbose keyboard debugging. Set via `set_keyboard_debug_enabled()`.
static KEYBOARD_DEBUG_ENABLED: AtomicBool = AtomicBool::new(cfg!(debug_assertions));

/// Enable or disable keyboard debug logging at runtime.
pub fn set_keyboard_debug_enabled(enabled: bool) {
    KEYBOARD_DEBUG_ENABLED.store(enabled, Ordering::SeqCst);
}

/// iOS-specific debug logging for keyboard events.
/// Writes to stderr and optionally to a log file in the temp directory.
#[allow(unused_variables)]
pub fn ios_keyboard_log(message: &str) {
    if !KEYBOARD_DEBUG_ENABLED.load(Ordering::SeqCst) {
        return;
    }

    let msg = format!("[iOS Keyboard] {}", message);
    eprintln!("{}", msg);

    // Also write to a file that can be retrieved from the simulator
    #[cfg(debug_assertions)]
    {
        use std::io::Write;
        let mut log_path = std::env::temp_dir();
        log_path.push("zed_ios_keyboard.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(file, "{}", msg);
        }
    }
}

/// UITouch phase constants
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UITouchPhase {
    Began = 0,
    Moved = 1,
    Stationary = 2,
    Ended = 3,
    Cancelled = 4,
}

/// UIPress phase constants (for hardware keyboard)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UIPressPhase {
    Began = 0,
    Changed = 1,
    Stationary = 2,
    Ended = 3,
    Cancelled = 4,
}

/// UIKeyModifierFlags
#[derive(Debug, Clone, Copy)]
pub struct UIKeyModifierFlags(pub i64);

impl UIKeyModifierFlags {
    pub const ALPHA_SHIFT: i64 = 1 << 16;  // Caps Lock
    pub const SHIFT: i64 = 1 << 17;
    pub const CONTROL: i64 = 1 << 18;
    pub const ALTERNATE: i64 = 1 << 19;   // Option
    pub const COMMAND: i64 = 1 << 20;
    pub const NUMERIC_PAD: i64 = 1 << 21;

    pub fn contains(&self, flag: i64) -> bool {
        (self.0 & flag) != 0
    }

    pub fn to_modifiers(&self) -> Modifiers {
        Modifiers {
            control: self.contains(Self::CONTROL),
            alt: self.contains(Self::ALTERNATE),
            shift: self.contains(Self::SHIFT),
            platform: self.contains(Self::COMMAND),  // Command key on Apple platforms
            function: false,
        }
    }
}

/// Translate a UITouch to a GPUI mouse event.
///
/// Single touches are treated as left mouse button events.
/// This provides basic interaction support with the existing mouse-based UI.
pub fn translate_touch_to_mouse(
    touch: *mut Object,
    view: *mut Object,
    modifiers: Modifiers,
) -> Option<PlatformInput> {
    unsafe {
        let phase: i64 = msg_send![touch, phase];
        let phase = match phase {
            0 => UITouchPhase::Began,
            1 => UITouchPhase::Moved,
            2 => UITouchPhase::Stationary,
            3 => UITouchPhase::Ended,
            4 => UITouchPhase::Cancelled,
            _ => return None,
        };

        let location: CGPoint = msg_send![touch, locationInView: view];
        let tap_count: usize = msg_send![touch, tapCount];
        let position = point(px(location.x as f32), px(location.y as f32));

        match phase {
            UITouchPhase::Began => Some(PlatformInput::MouseDown(MouseDownEvent {
                position,
                button: MouseButton::Left,
                click_count: tap_count,
                modifiers,
                first_mouse: false,
            })),
            UITouchPhase::Moved => Some(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers,
            })),
            UITouchPhase::Ended | UITouchPhase::Cancelled => {
                Some(PlatformInput::MouseUp(MouseUpEvent {
                    position,
                    button: MouseButton::Left,
                    click_count: tap_count,
                    modifiers,
                }))
            }
            UITouchPhase::Stationary => None,
        }
    }
}

/// Translate a UIPanGestureRecognizer state to a scroll event.
pub fn translate_pan_to_scroll(
    gesture: *mut Object,
    view: *mut Object,
    modifiers: Modifiers,
) -> Option<PlatformInput> {
    unsafe {
        let state: i64 = msg_send![gesture, state];
        let location: CGPoint = msg_send![gesture, locationInView: view];
        let translation: CGPoint = msg_send![gesture, translationInView: view];

        let position = point(px(location.x as f32), px(location.y as f32));
        let delta = point(px(translation.x as f32), px(translation.y as f32));

        // Reset the translation so we get incremental deltas
        let zero = CGPoint { x: 0.0, y: 0.0 };
        let _: () = msg_send![gesture, setTranslation: zero inView: view];

        let touch_phase = match state {
            1 => GpuiTouchPhase::Started,  // UIGestureRecognizerStateBegan
            2 => GpuiTouchPhase::Moved,    // UIGestureRecognizerStateChanged
            3 | 4 => GpuiTouchPhase::Ended, // UIGestureRecognizerStateEnded/Cancelled
            _ => return None,
        };

        Some(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta: crate::ScrollDelta::Pixels(delta),
            modifiers,
            touch_phase,
        }))
    }
}

/// Translate a UIKey press to a GPUI keyboard event.
///
/// This function safely extracts key information from a UIPress object,
/// handling null pointers and invalid data gracefully.
pub fn translate_key_press(
    press: *mut Object,
    is_key_down: bool,
    is_repeat: bool,
) -> Option<PlatformInput> {
    unsafe {
        // Validate press object
        if press.is_null() {
            ios_keyboard_log("translate_key_press: press is null");
            return None;
        }

        let key: *mut Object = msg_send![press, key];
        if key.is_null() {
            ios_keyboard_log("translate_key_press: press.key is null (not a keyboard event?)");
            return None;
        }

        let key_code: i64 = msg_send![key, keyCode];
        ios_keyboard_log(&format!(
            "Key event: code={} (0x{:x}), down={}, repeat={}",
            key_code, key_code, is_key_down, is_repeat
        ));

        // Safely extract modifier flags
        let modifier_flags: i64 = msg_send![key, modifierFlags];
        let modifiers = UIKeyModifierFlags(modifier_flags).to_modifiers();
        // Safely get the character string from the key
        let key_str = get_key_characters_safely(key);
        if let Some(ref s) = key_str {
            ios_keyboard_log(&format!("Key characters: '{}'", s));
        }

        // Determine the key name: use characters if available, otherwise map from code
        let key_name = match &key_str {
            Some(s) if !s.is_empty() && !is_special_key_code(key_code) => s.clone(),
            _ => {
                let mapped = key_code_to_string(key_code);
                ios_keyboard_log(&format!("Mapped key code {} to '{}'", key_code, mapped));
                mapped
            }
        };

        // For character input, only use key_char for printable characters
        let key_char =
            key_str.filter(|s| !s.is_empty() && s.chars().all(|c| !c.is_control() || c == '\t'));

        let keystroke = Keystroke {
            key: key_name.into(),
            modifiers,
            key_char,
        };

        ios_keyboard_log(&format!(
            "Created keystroke: key='{}', char={:?}, mods={:?}",
            keystroke.key, keystroke.key_char, keystroke.modifiers
        ));

        if is_key_down {
            Some(PlatformInput::KeyDown(KeyDownEvent {
                keystroke,
                is_held: is_repeat,
                prefer_character_input: false,
            }))
        } else {
            Some(PlatformInput::KeyUp(KeyUpEvent { keystroke }))
        }
    }
}

/// Safely extract the characters string from a UIKey object.
/// Returns None if the string is null, empty, or cannot be converted to UTF-8.
unsafe fn get_key_characters_safely(key: *mut Object) -> Option<String> {
    if key.is_null() {
        return None;
    }

    let characters: *mut Object = msg_send![key, characters];
    if characters.is_null() {
        ios_keyboard_log("get_key_characters_safely: characters is null");
        return None;
    }

    // Check length before attempting to get UTF8String
    let len: usize = msg_send![characters, length];
    if len == 0 {
        ios_keyboard_log("get_key_characters_safely: characters length is 0");
        return None;
    }

    let c_str: *const i8 = msg_send![characters, UTF8String];
    if c_str.is_null() {
        ios_keyboard_log("get_key_characters_safely: UTF8String returned null");
        return None;
    }

    match std::ffi::CStr::from_ptr(c_str).to_str() {
        Ok(s) => Some(s.to_string()),
        Err(e) => {
            ios_keyboard_log(&format!("get_key_characters_safely: UTF-8 error: {}", e));
            None
        }
    }
}

/// Check if a key code represents a special key that should use the mapped name
/// rather than the characters string.
fn is_special_key_code(key_code: i64) -> bool {
    matches!(
        key_code,
        0x28 | // Enter
        0x29 | // Escape
        0x2A | // Backspace
        0x2B | // Tab
        0x2C | // Space
        0x39 | // Caps Lock
        0x3A..=0x45 | // F1-F12
        0x46..=0x48 | // PrintScreen, ScrollLock, Pause
        0x49..=0x52 | // Insert, Home, PageUp, Delete, End, PageDown, Arrows
        0x53..=0x63 | // Keypad
        0xE0..=0xE7   // Modifiers
    )
}

/// Translate a modifier flags change to a modifiers changed event.
pub fn translate_modifiers_changed(modifier_flags: i64) -> PlatformInput {
    let modifiers = UIKeyModifierFlags(modifier_flags).to_modifiers();
    PlatformInput::ModifiersChanged(ModifiersChangedEvent { 
        modifiers,
        capslock: Capslock { on: UIKeyModifierFlags(modifier_flags).contains(UIKeyModifierFlags::ALPHA_SHIFT) },
    })
}

/// Map iOS key codes (UIKeyboardHIDUsage) to key strings.
/// Based on USB HID Usage Tables.
fn key_code_to_string(key_code: i64) -> String {
    match key_code {
        // Letters
        0x04..=0x1D => {
            let c = (b'a' + (key_code - 0x04) as u8) as char;
            c.to_string()
        }
        // Numbers
        0x1E..=0x26 => {
            let c = (b'1' + (key_code - 0x1E) as u8) as char;
            c.to_string()
        }
        0x27 => "0".to_string(),
        
        // Special keys
        0x28 => "enter".to_string(),
        0x29 => "escape".to_string(),
        0x2A => "backspace".to_string(),
        0x2B => "tab".to_string(),
        0x2C => "space".to_string(),
        0x2D => "-".to_string(),
        0x2E => "=".to_string(),
        0x2F => "[".to_string(),
        0x30 => "]".to_string(),
        0x31 => "\\".to_string(),
        0x33 => ";".to_string(),
        0x34 => "'".to_string(),
        0x35 => "`".to_string(),
        0x36 => ",".to_string(),
        0x37 => ".".to_string(),
        0x38 => "/".to_string(),
        0x39 => "capslock".to_string(),
        // Function keys
        0x3A => "f1".to_string(),
        0x3B => "f2".to_string(),
        0x3C => "f3".to_string(),
        0x3D => "f4".to_string(),
        0x3E => "f5".to_string(),
        0x3F => "f6".to_string(),
        0x40 => "f7".to_string(),
        0x41 => "f8".to_string(),
        0x42 => "f9".to_string(),
        0x43 => "f10".to_string(),
        0x44 => "f11".to_string(),
        0x45 => "f12".to_string(),
        // System keys
        0x46 => "printscreen".to_string(),
        0x47 => "scrolllock".to_string(),
        0x48 => "pause".to_string(),
        // Navigation
        0x49 => "insert".to_string(),
        0x4A => "home".to_string(),
        0x4B => "pageup".to_string(),
        0x4C => "delete".to_string(),
        0x4D => "end".to_string(),
        0x4E => "pagedown".to_string(),
        0x4F => "right".to_string(),
        0x50 => "left".to_string(),
        0x51 => "down".to_string(),
        0x52 => "up".to_string(),
        
        // Modifiers (these usually don't come through as key events)
        0xE0 => "control".to_string(),
        0xE1 => "shift".to_string(),
        0xE2 => "alt".to_string(),
        0xE3 => "cmd".to_string(),
        0xE4 => "control".to_string(), // Right control
        0xE5 => "shift".to_string(),   // Right shift
        0xE6 => "alt".to_string(),     // Right alt
        0xE7 => "cmd".to_string(),     // Right cmd

        // Keypad keys
        0x53 => "numlock".to_string(),
        0x54 => "keypad/".to_string(),
        0x55 => "keypad*".to_string(),
        0x56 => "keypad-".to_string(),
        0x57 => "keypad+".to_string(),
        0x58 => "keypadenter".to_string(),
        0x59 => "keypad1".to_string(),
        0x5A => "keypad2".to_string(),
        0x5B => "keypad3".to_string(),
        0x5C => "keypad4".to_string(),
        0x5D => "keypad5".to_string(),
        0x5E => "keypad6".to_string(),
        0x5F => "keypad7".to_string(),
        0x60 => "keypad8".to_string(),
        0x61 => "keypad9".to_string(),
        0x62 => "keypad0".to_string(),
        0x63 => "keypad.".to_string(),
        _ => format!("unknown-{}", key_code),
    }
}
