//! iOS platform implementation for GPUI.
//!
//! This module provides the iOS-specific platform layer, enabling GPUI to run on iPadOS.
//! It leverages the existing Metal renderer and provides UIKit integration
//! for window management, input handling, and system services.
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

// iOS-specific Metal renderer (adapted from macOS, no core_video dependency)
pub mod metal_renderer;

// metal_atlas has no cocoa dependencies, can be shared via #[path]
#[path = "../mac/metal_atlas.rs"]
mod metal_atlas;

use metal_renderer as renderer;

// iOS-specific open_type module (adapted from macOS, local CGFloat)
#[cfg(feature = "font-kit")]
mod open_type;

// iOS-specific text system (adapted from macOS, local CGFloat, geometry::CGPoint)
#[cfg(feature = "font-kit")]
mod text_system;

use crate::{DevicePixels, Pixels, Size, px, size};
use objc::{
    Encode, Encoding,
    msg_send,
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
    /// Convert a Rust `bool` into an Objective-C `BOOL`.
    ///
    /// # Examples
    ///
    /// ```
    /// // `to_objc` is provided by the `BoolExt` trait implemented for `bool`.
    /// assert_eq!(true.to_objc(), objc::runtime::YES);
    /// assert_eq!(false.to_objc(), objc::runtime::NO);
    /// ```
    fn to_objc(self) -> BOOL {
        if self { YES } else { NO }
    }
}

/// Create an Objective-C `NSString` from a Rust `&str`.
///
/// The returned pointer is an autoreleased `NSString` object and is valid while the current
/// Objective-C autorelease pool is in effect. The input `string` is encoded as UTF-8.
///
/// # Safety
///
/// The caller must ensure an Objective-C runtime and an active autorelease pool exist when this
/// function is called. The returned pointer is a raw Objective-C object and must be used with
/// Objective-C messaging conventions.
///
/// # Examples
///
/// ```
/// # use std::ffi::c_void;
/// # unsafe {
/// let s = ns_string("hello");
/// assert!(!s.is_null());
/// # }
/// ```
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
    /// Objective-C type encoding for `CGSize`.
    ///
    /// Returns an `Encoding` representing the Objective-C type string "{CGSize=dd}".
    ///
    /// # Examples
    ///
    /// ```
    /// let _enc = CGSize::encode();
    /// ```
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGSize=dd}") }
    }
}

impl From<CGSize> for Size<Pixels> {
    /// Convert a `CGSize` to a `Size<Pixels>`.
    ///
    /// The `width` and `height` are cast to `f32` and converted to pixel units via `px`.
    ///
    /// # Examples
    ///
    /// ```
    /// let cg = CGSize { width: 10.0, height: 20.0 };
    /// let size: Size<Pixels> = Size::from(cg);
    /// assert_eq!(size.width.get(), 10.0f32);
    /// assert_eq!(size.height.get(), 20.0f32);
    /// ```
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
    /// Objective-C runtime type encoding for `CGRect`.
    ///
    /// # Examples
    ///
    /// ```
    /// let enc = encode();
    /// // `enc` encodes the Objective-C layout "{CGRect={CGPoint=dd}{CGSize=dd}}"
    /// ```
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
    /// Returns the Objective-C type encoding for `CGPoint`.
    ///
    /// # Examples
    ///
    /// ```
    /// let enc = encode();
    /// assert_eq!(enc, unsafe { objc::encode::Encoding::from_str("{CGPoint=dd}") });
    /// ```
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGPoint=dd}") }
    }
}

impl From<CGRect> for Size<Pixels> {
    /// Create a `Size<Pixels>` from a `CGRect`'s size.
    ///
    /// # Examples
    ///
    /// ```
    /// let rect = CGRect {
    ///     origin: CGPoint { x: 0.0, y: 0.0 },
    ///     size: CGSize { width: 100.0, height: 50.0 },
    /// };
    /// let _size: Size<Pixels> = rect.into();
    /// ```
    fn from(rect: CGRect) -> Self {
        size(px(rect.size.width as f32), px(rect.size.height as f32))
    }
}

impl From<CGRect> for Size<DevicePixels> {
    /// Create a `Size<DevicePixels>` from a `CGRect` by converting the rect's width and height to device pixels.
    ///
    /// # Examples
    ///
    /// ```
    /// let rect = CGRect {
    ///     origin: CGPoint { x: 0.0, y: 0.0 },
    ///     size: CGSize { width: 100.0, height: 200.0 },
    /// };
    /// let size: Size<DevicePixels> = rect.into();
    /// let Size { width, height } = size;
    /// assert_eq!(width, DevicePixels(100));
    /// assert_eq!(height, DevicePixels(200));
    /// ```
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