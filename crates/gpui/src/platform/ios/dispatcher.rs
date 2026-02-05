//! iOS dispatcher implementation using Grand Central Dispatch (GCD).
//!
//! This is nearly identical to the macOS dispatcher since GCD is available on both platforms.

use crate::{PlatformDispatcher, TaskLabel};
use async_task::Runnable;
use objc::{
    class, msg_send,
    runtime::{BOOL, YES},
    sel, sel_impl,
};
use parking::{Parker, Unparker};
use parking_lot::Mutex;
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::Arc,
    time::Duration,
};

// GCD type definitions - these are the same on iOS and macOS
type dispatch_queue_t = *mut c_void;
type dispatch_time_t = u64;
type dispatch_function_t = Option<unsafe extern "C" fn(*mut c_void)>;

const DISPATCH_TIME_NOW: u64 = 0;
const QOS_CLASS_USER_INITIATED: u32 = 0x19;

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    fn dispatch_get_main_queue() -> dispatch_queue_t;
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> dispatch_queue_t;
    fn dispatch_async_f(queue: dispatch_queue_t, context: *mut c_void, work: dispatch_function_t);
    fn dispatch_after_f(
        when: dispatch_time_t,
        queue: dispatch_queue_t,
        context: *mut c_void,
        work: dispatch_function_t,
    );
    fn dispatch_time(when: dispatch_time_t, delta: i64) -> dispatch_time_t;
}

pub(crate) struct IosDispatcher {
    parker: Arc<Mutex<Parker>>,
}

impl Default for IosDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl IosDispatcher {
    pub fn new() -> Self {
        IosDispatcher {
            parker: Arc::new(Mutex::new(Parker::new())),
        }
    }
}

impl PlatformDispatcher for IosDispatcher {
    fn is_main_thread(&self) -> bool {
        let is_main_thread: BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
        is_main_thread == YES
    }

    fn dispatch(&self, runnable: Runnable, _label: Option<TaskLabel>) {
        unsafe {
            dispatch_async_f(
                dispatch_get_global_queue(QOS_CLASS_USER_INITIATED as isize, 0),
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline),
            );
        }
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        unsafe {
            dispatch_async_f(
                dispatch_get_main_queue(),
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline),
            );
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
        unsafe {
            let queue = dispatch_get_global_queue(QOS_CLASS_USER_INITIATED as isize, 0);
            let when = dispatch_time(DISPATCH_TIME_NOW, duration.as_nanos() as i64);
            dispatch_after_f(
                when,
                queue,
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline),
            );
        }
    }

    fn park(&self, timeout: Option<Duration>) -> bool {
        if let Some(timeout) = timeout {
            self.parker.lock().park_timeout(timeout)
        } else {
            self.parker.lock().park();
            true
        }
    }

    fn unparker(&self) -> Unparker {
        self.parker.lock().unparker()
    }
}

unsafe extern "C" fn trampoline(runnable: *mut c_void) {
    let task = unsafe { Runnable::<()>::from_raw(NonNull::new_unchecked(runnable as *mut ())) };
    task.run();
}
