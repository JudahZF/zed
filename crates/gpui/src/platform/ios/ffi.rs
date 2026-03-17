//! FFI layer for iOS app delegate to call into GPUI.
//!
//! This module provides C-callable functions that the iOS app delegate (written in
//! Objective-C or Swift) can use to interact with the GPUI application lifecycle.
//!
//! Note: The actual platform initialization happens through the standard `App::new()`
//! pathway. These FFI functions are for lifecycle notifications only.

use crate::{IosLifecycleEvent, Subscription};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::{cell::RefCell, collections::BTreeMap, ffi::CStr};

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static APP_LIFECYCLE_STATE: AtomicU8 = AtomicU8::new(AppLifecycleState::NotInitialized as u8);

thread_local! {
    static IOS_LIFECYCLE_OBSERVERS: RefCell<IosLifecycleObserverRegistry> =
        RefCell::new(IosLifecycleObserverRegistry::default());
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum AppLifecycleState {
    NotInitialized = 0,
    Inactive = 1,
    Active = 2,
    Background = 3,
    Terminating = 4,
}

#[derive(Default)]
struct IosLifecycleObserverRegistry {
    observers: Option<BTreeMap<usize, Box<dyn FnMut(IosLifecycleEvent)>>>,
    pending_observers: BTreeMap<usize, Box<dyn FnMut(IosLifecycleEvent)>>,
    dropped_during_dispatch: Vec<usize>,
    next_observer_id: usize,
}

fn set_lifecycle_state(state: AppLifecycleState) {
    APP_LIFECYCLE_STATE.store(state as u8, Ordering::SeqCst);
}

fn is_initialized() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

pub(crate) fn observe_ios_lifecycle(
    callback: impl FnMut(IosLifecycleEvent) + 'static,
) -> Subscription {
    IOS_LIFECYCLE_OBSERVERS.with(|observers| {
        let mut observers = observers.borrow_mut();
        let observer_id = observers.next_observer_id;
        observers.next_observer_id += 1;

        let callback = Box::new(callback) as Box<dyn FnMut(IosLifecycleEvent)>;
        if let Some(active_observers) = observers.observers.as_mut() {
            active_observers.insert(observer_id, callback);
        } else {
            observers.pending_observers.insert(observer_id, callback);
        }

        Subscription::new(move || {
            IOS_LIFECYCLE_OBSERVERS.with(|observers| {
                let mut observers = observers.borrow_mut();
                if let Some(active_observers) = observers.observers.as_mut() {
                    active_observers.remove(&observer_id);
                } else if observers.pending_observers.remove(&observer_id).is_none() {
                    observers.dropped_during_dispatch.push(observer_id);
                }
            });
        })
    })
}

fn dispatch_lifecycle_event(event: IosLifecycleEvent) {
    IOS_LIFECYCLE_OBSERVERS.with(|observers| {
        let mut callbacks = {
            let mut observers = observers.borrow_mut();
            observers.observers.take().unwrap_or_default()
        };

        for callback in callbacks.values_mut() {
            callback(event);
        }

        let mut observers = observers.borrow_mut();
        for observer_id in observers.dropped_during_dispatch.drain(..) {
            callbacks.remove(&observer_id);
        }
        callbacks.append(&mut observers.pending_observers);
        observers.observers = Some(callbacks);
    });
}

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
    set_lifecycle_state(AppLifecycleState::Inactive);

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
    if !is_initialized() {
        return;
    }
    // Frame requests are handled via CADisplayLink callbacks in window.rs.
    // This hook remains for app delegates that want explicit frame nudges.
}

/// Notify GPUI that the app has become active.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_did_become_active() {
    if !is_initialized() {
        return;
    }
    set_lifecycle_state(AppLifecycleState::Active);
    dispatch_lifecycle_event(IosLifecycleEvent::DidBecomeActive);
}

/// Notify GPUI that the app will resign active.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_resign_active() {
    if !is_initialized() {
        return;
    }
    set_lifecycle_state(AppLifecycleState::Inactive);
    dispatch_lifecycle_event(IosLifecycleEvent::WillResignActive);
}

/// Notify GPUI that the app did enter background.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_did_enter_background() {
    if !is_initialized() {
        return;
    }
    set_lifecycle_state(AppLifecycleState::Background);
    dispatch_lifecycle_event(IosLifecycleEvent::DidEnterBackground);
}

/// Notify GPUI that the app will enter foreground.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_enter_foreground() {
    if !is_initialized() {
        return;
    }
    set_lifecycle_state(AppLifecycleState::Inactive);
    dispatch_lifecycle_event(IosLifecycleEvent::WillEnterForeground);
}

/// Notify GPUI that the app will terminate.
///
/// # Safety
/// Must be called from the main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_will_terminate() {
    if !is_initialized() {
        return;
    }
    set_lifecycle_state(AppLifecycleState::Terminating);
    dispatch_lifecycle_event(IosLifecycleEvent::WillTerminate);
}

/// Handle a URL opened by the system.
///
/// # Safety
/// `url` must be a valid null-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gpui_ios_open_url(url: *const i8) {
    if !is_initialized() {
        return;
    }
    if url.is_null() {
        log::warn!("gpui_ios_open_url called with null pointer");
        return;
    }

    let parse_result = unsafe { CStr::from_ptr(url) }.to_str().map(str::to_string);
    match parse_result {
        Ok(opened_url) => {
            log::debug!("Received open-url callback for iOS app: {opened_url}");
        }
        Err(err) => {
            log::warn!("gpui_ios_open_url received invalid UTF-8 URL: {err}");
        }
    }
}
