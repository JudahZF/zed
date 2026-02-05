//! iOS platform implementation for GPUI.
//!
//! This module provides the iOS-specific platform layer, enabling GPUI to run on iPadOS.
//! It uses the Blade renderer (WGSL shaders) instead of raw Metal shaders to avoid
//! the [[clip_distance]] feature which is not supported on iOS Simulator.
//!
//! Key differences from macOS:
//! - Uses UIWindow/UIView instead of NSWindow/NSView
//! - Touch input translated to mouse events
//! - No menu bar (set_menus is a no-op)
//! - No cursor styles (iOS doesn't have a visible cursor)
//! - Limited file system access (sandboxed)

mod dispatcher;
mod display;
mod events;
mod platform;
mod window;

// Use Blade renderer for iOS (WGSL shaders avoid Metal clip_distance issues)
use super::blade as renderer;

// iOS-specific open_type module (adapted from macOS, local CGFloat)
#[cfg(feature = "font-kit")]
mod open_type;

// iOS-specific text system (adapted from macOS, local CGFloat, geometry::CGPoint)
#[cfg(feature = "font-kit")]
mod text_system;

use crate::{DevicePixels, Pixels, Size, px, size};
use objc::{
    Encode, Encoding, msg_send,
    runtime::{BOOL, NO, YES},
    sel, sel_impl,
};
use std::ffi::c_void;

pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
pub(crate) use window::*;

#[cfg(feature = "font-kit")]
pub(crate) use text_system::*;

/// Placeholder for screen capture frame - iOS may support this differently
pub(crate) type PlatformScreenCaptureFrame = ();

/// Extension trait for converting Rust bools to Objective-C BOOLs
trait BoolExt {
    fn to_objc(self) -> BOOL;
}

impl BoolExt for bool {
    fn to_objc(self) -> BOOL {
        if self { YES } else { NO }
    }
}

/// Helper to create an NSString from a Rust string slice
unsafe fn ns_string(string: &str) -> *mut objc::runtime::Object {
    use objc::class;
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
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct CGSize {
    pub width: f64,
    pub height: f64,
}

// CGSize encoding for objc - {CGSize=dd} means struct with two doubles
unsafe impl Encode for CGSize {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGSize=dd}") }
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

// CGRect encoding for objc
unsafe impl Encode for CGRect {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

/// CGPoint structure
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct CGPoint {
    pub x: f64,
    pub y: f64,
}

// CGPoint encoding for objc
unsafe impl Encode for CGPoint {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGPoint=dd}") }
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
