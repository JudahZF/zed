//! iOS display (UIScreen) implementation.
//!
//! On iOS, UIScreen represents the device's display. Unlike macOS, iOS devices
//! typically have a single built-in display, though external displays are possible.

use crate::{Bounds, DisplayId, Pixels, PlatformDisplay, px, size};
use anyhow::{Result, anyhow};
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use uuid::Uuid;

use super::{CGRect, CGSize};

/// Wrapper around UIScreen for iOS display handling.
#[derive(Debug)]
pub(crate) struct IosDisplay {
    screen: *mut Object,
}

// UIScreen can be sent between threads when we're just reading properties
unsafe impl Send for IosDisplay {}
unsafe impl Sync for IosDisplay {}

impl IosDisplay {
    /// Obtain the device's built-in main screen wrapped as an `IosDisplay`.
    ///
    /// Returns an `IosDisplay` that wraps the primary device `UIScreen`.
    ///
    /// # Examples
    ///
    /// ```
    /// let main = IosDisplay::main();
    /// // `main` represents the primary device screen; scale should be >= 1.0 on iOS devices
    /// assert!(main.scale() >= 1.0);
    /// ```
    pub fn main() -> Self {
        unsafe {
            let screen: *mut Object = msg_send![class!(UIScreen), mainScreen];
            Self { screen }
        }
    }

    /// Enumerates all available screens (main and any external displays) and returns them as `IosDisplay` instances.
    ///
    /// # Examples
    ///
    /// ```
    /// let displays = gpui::platform::ios::display::IosDisplay::all();
    /// assert!(!displays.is_empty());
    /// ```
    pub fn all() -> Vec<Self> {
        unsafe {
            let screens: *mut Object = msg_send![class!(UIScreen), screens];
            let count: usize = msg_send![screens, count];
            
            (0..count)
                .map(|i| {
                    let screen: *mut Object = msg_send![screens, objectAtIndex: i];
                    Self { screen }
                })
                .collect()
        }
    }

    /// Provide the screen's native bounds in pixels.
    ///
    /// # Returns
    ///
    /// The screen's native bounding rectangle in pixels as a `CGRect`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let screen = IosDisplay::main();
    /// let native = screen.native_bounds();
    /// // `native` is a CGRect representing the screen size in physical pixels.
    /// ```
    pub fn native_bounds(&self) -> CGRect {
        unsafe { msg_send![self.screen, nativeBounds] }
    }

    /// The screen's bounds measured in points.
    ///
    /// Coordinates and dimensions are expressed in UIKit points (not pixels).
    ///
    /// # Examples
    ///
    /// ```
    /// let bounds = IosDisplay::main().bounds_in_points();
    /// assert!(bounds.size.width > 0.0);
    /// ```
    pub fn bounds_in_points(&self) -> CGRect {
        unsafe { msg_send![self.screen, bounds] }
    }

    /// Retrieve the screen's scale factor (pixel-per-point ratio).
    ///
    /// # Examples
    ///
    /// ```
    /// let scale = IosDisplay::main().scale();
    /// assert!(scale >= 1.0);
    /// ```
    pub fn scale(&self) -> f64 {
        unsafe { msg_send![self.screen, scale] }
    }
}

impl PlatformDisplay for IosDisplay {
    /// Computes a stable display identifier based on the wrapped UIScreen pointer.
    ///
    /// The identifier is produced by hashing the raw screen pointer; it is stable for the
    /// lifetime of the running application but not guaranteed persistent across launches.
    ///
    /// # Examples
    ///
    /// ```
    /// let display = IosDisplay::main();
    /// let id1 = display.id();
    /// let id2 = display.id();
    /// assert_eq!(id1, id2);
    /// ```
    fn id(&self) -> DisplayId {
        // iOS doesn't have a direct equivalent to CGDirectDisplayID,
        // so we use a hash of the screen pointer as an identifier.
        // This is stable for the lifetime of the app.
        // Use lower bits of pointer hash to avoid truncation issues
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (self.screen as usize).hash(&mut hasher);
        DisplayId(hasher.finish() as u32)
    }

    /// Generates a deterministic UUID for this screen from its native pixel bounds and scale.
    ///
    /// The UUID is derived from the screen's nativeBounds (width and height) and scale and is stable for the lifetime of the process.
    ///
    /// # Returns
    ///
    /// `Ok(Uuid)` containing the generated UUID.
    ///
    /// # Examples
    ///
    /// ```
    /// // Example: obtain a UUID for the main screen
    /// let display = IosDisplay::main();
    /// let id = display.uuid().unwrap();
    /// assert_ne!(id, uuid::Uuid::nil());
    /// ```
    fn uuid(&self) -> Result<Uuid> {
        // iOS doesn't provide persistent display UUIDs like macOS does.
        // We generate a deterministic UUID based on the screen properties.
        // This won't persist across app launches but is stable within a session.
        unsafe {
            let bounds: CGRect = msg_send![self.screen, nativeBounds];
            let scale: f64 = msg_send![self.screen, scale];
            
            // Create a reproducible UUID from screen properties
            let data = format!(
                "ios-screen-{}-{}-{}",
                bounds.size.width as u32,
                bounds.size.height as u32,
                (scale * 100.0) as u32
            );
            
            Ok(Uuid::new_v5(&Uuid::NAMESPACE_OID, data.as_bytes()))
        }
    }

    /// Converts the display's point-based bounds into pixel-based bounds using the display scale.
    ///
    /// # Returns
    ///
    /// A `Bounds<Pixels>` where width and height are the point bounds multiplied by the display scale,
    /// and the origin is the default (zero).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// let display = IosDisplay::main();
    /// let pixel_bounds = display.bounds();
    /// // pixel_bounds.size.width and pixel_bounds.size.height are in pixels
    /// ```
    fn bounds(&self) -> Bounds<Pixels> {
        let bounds = self.bounds_in_points();
        let scale = self.scale() as f32;
        Bounds {
            origin: Default::default(),
            size: size(
                px(bounds.size.width as f32 * scale),
                px(bounds.size.height as f32 * scale),
            ),
        }
    }
}