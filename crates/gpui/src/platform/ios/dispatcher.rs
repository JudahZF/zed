//! iOS dispatcher implementation using Grand Central Dispatch (GCD).
//!
//! This is nearly identical to the macOS dispatcher since GCD is available on both platforms.

use crate::{
    GLOBAL_THREAD_TIMINGS, PlatformDispatcher, Priority, RunnableMeta, RunnableVariant,
    THREAD_TIMINGS, TaskTiming, ThreadTaskTimings,
};
use async_task::Runnable;
use objc::{
    class, msg_send,
    runtime::{BOOL, YES},
    sel, sel_impl,
};
use std::{
    ffi::c_void,
    ptr::{NonNull, addr_of},
    time::{Duration, Instant},
};

// GCD type definitions - these are the same on iOS and macOS
#[allow(non_camel_case_types)]
type dispatch_queue_t = *mut c_void;
#[allow(non_camel_case_types)]
type dispatch_time_t = u64;
#[allow(non_camel_case_types)]
type dispatch_function_t = Option<unsafe extern "C" fn(*mut c_void)>;

const DISPATCH_TIME_NOW: u64 = 0;
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;
const DISPATCH_QUEUE_PRIORITY_DEFAULT: isize = 0;
const DISPATCH_QUEUE_PRIORITY_LOW: isize = -2;

#[link(name = "System", kind = "dylib")]
unsafe extern "C" {
    // Global variable for the main queue - not a function
    static _dispatch_main_q: c_void;

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

/// Get the main dispatch queue.
/// On iOS/macOS, dispatch_get_main_queue() is a macro that accesses _dispatch_main_q.
fn dispatch_get_main_queue() -> dispatch_queue_t {
    unsafe { addr_of!(_dispatch_main_q) as dispatch_queue_t }
}
pub(crate) struct IosDispatcher;

impl Default for IosDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl IosDispatcher {
    pub fn new() -> Self {
        IosDispatcher
    }
}

impl PlatformDispatcher for IosDispatcher {
    fn get_all_timings(&self) -> Vec<ThreadTaskTimings> {
        let global_timings = GLOBAL_THREAD_TIMINGS.lock();
        ThreadTaskTimings::convert(&global_timings)
    }

    fn get_current_thread_timings(&self) -> ThreadTaskTimings {
        THREAD_TIMINGS.with(|timings| {
            let timings = timings.lock();
            let thread_name = timings.thread_name.clone();
            let total_pushed = timings.total_pushed;
            let timings = &timings.timings;

            let mut vec = Vec::with_capacity(timings.len());

            let (s1, s2) = timings.as_slices();
            vec.extend_from_slice(s1);
            vec.extend_from_slice(s2);

            ThreadTaskTimings {
                thread_name,
                thread_id: std::thread::current().id(),
                timings: vec,
                total_pushed,
            }
        })
    }

    fn is_main_thread(&self) -> bool {
        let is_main_thread: BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
        is_main_thread == YES
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        let context = runnable.into_raw().as_ptr() as *mut c_void;
        let queue_priority = match priority {
            Priority::RealtimeAudio => {
                panic!("RealtimeAudio priority should use spawn_realtime, not dispatch")
            }
            Priority::High => DISPATCH_QUEUE_PRIORITY_HIGH,
            Priority::Medium => DISPATCH_QUEUE_PRIORITY_DEFAULT,
            Priority::Low => DISPATCH_QUEUE_PRIORITY_LOW,
        };

        unsafe {
            dispatch_async_f(
                dispatch_get_global_queue(queue_priority, 0),
                context,
                Some(trampoline),
            );
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let context = runnable.into_raw().as_ptr() as *mut c_void;
        unsafe {
            dispatch_async_f(dispatch_get_main_queue(), context, Some(trampoline));
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let context = runnable.into_raw().as_ptr() as *mut c_void;
        unsafe {
            let queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0);
            let when = dispatch_time(DISPATCH_TIME_NOW, duration.as_nanos() as i64);
            dispatch_after_f(when, queue, context, Some(trampoline));
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            f();
        });
    }
}

extern "C" fn trampoline(runnable: *mut c_void) {
    let runnable =
        unsafe { Runnable::<RunnableMeta>::from_raw(NonNull::new_unchecked(runnable as *mut ())) };
    let location = runnable.metadata().location;

    let start = Instant::now();
    let timing = TaskTiming {
        location,
        start,
        end: None,
    };

    THREAD_TIMINGS.with(|timings| {
        let mut timings = timings.lock();
        let timings = &mut timings.timings;
        if let Some(last_timing) = timings.iter_mut().rev().next() {
            if last_timing.location == timing.location {
                return;
            }
        }

        timings.push_back(timing);
    });

    runnable.run();
    let end = Instant::now();

    THREAD_TIMINGS.with(|timings| {
        let mut timings = timings.lock();
        let timings = &mut timings.timings;
        let Some(last_timing) = timings.iter_mut().rev().next() else {
            return;
        };
        last_timing.end = Some(end);
    });
}
