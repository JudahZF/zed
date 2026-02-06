//! FFI layer for iOS app delegate to call into GPUI.
//!
//! This module provides C-callable functions that the iOS app delegate (written in
//! Objective-C or Swift) can use to interact with the GPUI application lifecycle.
//!
//! Note: The actual platform initialization happens through the standard `App::new()`
//! pathway. These FFI functions are for lifecycle notifications only.

use std::sync::atomic::{AtomicBool, Ordering};

static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Initialize the GPUI platform for iOS.
///
/// This should be called early in the iOS app lifecycle, typically in
/// `application:didFinishLaunchingWithOptions:`.
///
/// # Safety
/// This function must only be called once from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_initialize() {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return;
    }

    // Initialize logging for iOS
    #[cfg(feature = "ios-log")]
    {
        use log::LevelFilter;
        use oslog::OsLogger;
        OsLogger::new("com.zed.gpui")
            .level_filter(LevelFilter::Debug)
            .init()
            .ok();
    }
}

/// Request a frame to be rendered.
///
/// Call this when CADisplayLink fires or when you need to update the display.
///
/// # Safety
/// Must be called from the main thread after `gpui_ios_initialize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_request_frame() {
    // Frame requests are handled via CADisplayLink callbacks in window.rs
}

/// Notify GPUI that the app has become active.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_did_become_active() {
    // Active state changes are handled via UIScene lifecycle notifications
}

/// Notify GPUI that the app will resign active.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_resign_active() {
    // Inactive state transitions
}

/// Notify GPUI that the app did enter background.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_did_enter_background() {
    // Background state - pause rendering, save state
}

/// Notify GPUI that the app will enter foreground.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_enter_foreground() {
    // Foreground state - resume rendering
}

/// Notify GPUI that the app will terminate.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_terminate() {
    // App termination
}

/// Handle a URL opened by the system.
///
/// # Safety
/// `url` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_open_url(_url: *const i8) {
    // URL handling is done through the platform's on_open_urls callback
}
