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
    /// Get the main screen (the device's built-in display).
    pub fn main() -> Self {
        unsafe {
            let screen: *mut Object = msg_send![class!(UIScreen), mainScreen];
            Self { screen }
        }
    }

    /// Get all available screens (main + any external displays).
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

    /// Get the native bounds of the screen in pixels.
    pub fn native_bounds(&self) -> CGRect {
        unsafe { msg_send![self.screen, nativeBounds] }
    }

    /// Get the bounds of the screen in points.
    pub fn bounds_in_points(&self) -> CGRect {
        unsafe { msg_send![self.screen, bounds] }
    }

    /// Get the scale factor of the screen (e.g., 2.0 for Retina, 3.0 for Super Retina).
    pub fn scale(&self) -> f64 {
        unsafe { msg_send![self.screen, scale] }
    }
}

impl PlatformDisplay for IosDisplay {
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
