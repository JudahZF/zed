//! iOS platform implementation for GPUI.
//!
//! iOS uses UIKit instead of AppKit, so the platform implementation differs
//! significantly from macOS despite sharing many underlying technologies:
//! - Grand Central Dispatch (GCD) for threading
//! - CoreText for text rendering
//! - Metal for GPU rendering
//! - CoreFoundation for many utilities

pub mod demos;
mod dispatcher;
mod display;
mod events;
pub mod ffi;
mod metal_renderer;
mod platform;
mod text_input;
mod window;

// Re-use the macOS text system since CoreText is available on iOS
#[cfg(feature = "font-kit")]
mod text_system;

mod metal_atlas;

// iOS uses a native Metal renderer with simulator-safe clipping (no Blade dependency).
use self::metal_renderer as renderer;

// iOS-specific open_type module (adapted from macOS, local CGFloat)
#[cfg(feature = "font-kit")]
mod open_type;

use crate::{DevicePixels, Pixels, Size, px, size};
use objc::runtime::{BOOL, NO, YES};
use std::ops::Range;

pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub use platform::*;
pub(crate) use window::*;

#[cfg(feature = "font-kit")]
pub(crate) use text_system::*;

/// Placeholder for iOS screen capture frame type.
/// iOS uses ReplayKit for screen capture, which would require additional implementation.
pub(crate) type PlatformScreenCaptureFrame = ();

trait BoolExt {
    fn to_objc(self) -> BOOL;
}

impl BoolExt for bool {
    fn to_objc(self) -> BOOL {
        if self { YES } else { NO }
    }
}

/// NSRange equivalent for iOS (same structure as macOS)
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct NSRange {
    pub location: usize,
    pub length: usize,
}

impl NSRange {
    pub fn invalid() -> Self {
        Self {
            location: usize::MAX,
            length: 0,
        }
    }

    pub fn is_valid(&self) -> bool {
        self.location != usize::MAX
    }

    pub fn to_range(self) -> Option<Range<usize>> {
        if self.is_valid() {
            let start = self.location;
            let end = start + self.length;
            Some(start..end)
        } else {
            None
        }
    }
}

impl From<Range<usize>> for NSRange {
    fn from(range: Range<usize>) -> Self {
        NSRange {
            location: range.start,
            length: range.len(),
        }
    }
}

unsafe impl objc::Encode for NSRange {
    fn encode() -> objc::Encoding {
        let encoding = format!(
            "{{NSRange={}{}}}",
            usize::encode().as_str(),
            usize::encode().as_str()
        );
        unsafe { objc::Encoding::from_str(&encoding) }
    }
}

/// CGSize structure for iOS coordinate handling
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct CGSize {
    pub width: f64,
    pub height: f64,
}

unsafe impl objc::Encode for CGSize {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGSize=dd}") }
    }
}

impl From<CGSize> for Size<Pixels> {
    fn from(value: CGSize) -> Self {
        Size {
            width: px(value.width as f32),
            height: px(value.height as f32),
        }
    }
}

/// CGRect structure for iOS coordinate handling
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct CGRect {
    pub origin: CGPoint,
    pub size: CGSize,
}

unsafe impl objc::Encode for CGRect {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

/// CGPoint structure
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct CGPoint {
    pub x: f64,
    pub y: f64,
}

unsafe impl objc::Encode for CGPoint {
    fn encode() -> objc::Encoding {
        unsafe { objc::Encoding::from_str("{CGPoint=dd}") }
    }
}

impl From<CGRect> for Size<Pixels> {
    fn from(rect: CGRect) -> Self {
        size(px(rect.size.width as f32), px(rect.size.height as f32))
    }
}

impl From<CGRect> for Size<DevicePixels> {
    fn from(rect: CGRect) -> Self {
        size(
            DevicePixels(rect.size.width as i32),
            DevicePixels(rect.size.height as i32),
        )
    }
}

/// UIEdgeInsets for safe area handling
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct UIEdgeInsets {
    pub top: f64,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
}

/// Helper to create an NSString from a Rust string slice
pub(crate) unsafe fn ns_string(string: &str) -> *mut objc::runtime::Object {
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::c_void;
    let ns_string: *mut objc::runtime::Object = msg_send![class!(NSString), alloc];
    let ns_string: *mut objc::runtime::Object = msg_send![
        ns_string,
        initWithBytes: string.as_ptr() as *const c_void
        length: string.len()
        encoding: 4u64 // NSUTF8StringEncoding
    ];
    let _: *mut objc::runtime::Object = msg_send![ns_string, autorelease];
    ns_string
}
