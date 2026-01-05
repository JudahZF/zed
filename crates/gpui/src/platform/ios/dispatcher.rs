//! iOS dispatcher implementation using Grand Central Dispatch (GCD).
//!
//! This is nearly identical to the macOS dispatcher since GCD is available on both platforms.

use crate::{
    PlatformDispatcher, Priority, RealtimePriority, RunnableMeta, RunnableVariant,
    TaskLabel, TaskTiming, ThreadTaskTimings, GLOBAL_THREAD_TIMINGS, THREAD_TIMINGS,
};
use async_task::Runnable;
use objc::{
    class, msg_send,
    runtime::{BOOL, YES},
    sel, sel_impl,
};
use std::{
    ffi::c_void,
    ptr::NonNull,
    time::{Duration, Instant},
};

// GCD type definitions - these are the same on iOS and macOS
type dispatch_queue_t = *mut c_void;
type dispatch_time_t = u64;
type dispatch_function_t = Option<unsafe extern "C" fn(*mut c_void)>;

const DISPATCH_TIME_NOW: u64 = 0;
const DISPATCH_QUEUE_PRIORITY_HIGH: isize = 2;
const DISPATCH_QUEUE_PRIORITY_DEFAULT: isize = 0;
const DISPATCH_QUEUE_PRIORITY_LOW: isize = -2;

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

    fn get_current_thread_timings(&self) -> Vec<TaskTiming> {
        THREAD_TIMINGS.with(|timings| {
            let timings = &timings.lock().timings;

            let mut vec = Vec::with_capacity(timings.len());

            let (s1, s2) = timings.as_slices();
            vec.extend_from_slice(s1);
            vec.extend_from_slice(s2);
            vec
        })
    }

    fn is_main_thread(&self) -> bool {
        let is_main_thread: BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
        is_main_thread == YES
    }

    fn dispatch(&self, runnable: RunnableVariant, _label: Option<TaskLabel>, priority: Priority) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
            ),
        };

        let queue_priority = match priority {
            Priority::Realtime(_) => unreachable!(),
            Priority::High => DISPATCH_QUEUE_PRIORITY_HIGH,
            Priority::Medium => DISPATCH_QUEUE_PRIORITY_DEFAULT,
            Priority::Low => DISPATCH_QUEUE_PRIORITY_LOW,
        };

        unsafe {
            dispatch_async_f(
                dispatch_get_global_queue(queue_priority, 0),
                context,
                trampoline,
            );
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
            ),
        };
        unsafe {
            dispatch_async_f(dispatch_get_main_queue(), context, trampoline);
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
            ),
        };
        unsafe {
            let queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0);
            let when = dispatch_time(DISPATCH_TIME_NOW, duration.as_nanos() as i64);
            dispatch_after_f(when, queue, context, trampoline);
        }
    }

    fn spawn_realtime(&self, _priority: RealtimePriority, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            f();
        });
    }
}

extern "C" fn trampoline(runnable: *mut c_void) {
    let task =
        unsafe { Runnable::<RunnableMeta>::from_raw(NonNull::new_unchecked(runnable as *mut ())) };

    let metadata = task.metadata();
    let location = metadata.location;

    if !metadata.is_app_alive() {
        drop(task);
        return;
    }

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

    task.run();
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

extern "C" fn trampoline_compat(runnable: *mut c_void) {
    let task = unsafe { Runnable::<()>::from_raw(NonNull::new_unchecked(runnable as *mut ())) };

    let location = core::panic::Location::caller();

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

    task.run();
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
