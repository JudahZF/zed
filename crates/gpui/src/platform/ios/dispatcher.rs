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
    panic::Location,
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

/// Payload for compat runnables that stores the source location captured at dispatch time.
struct RunnableCompatPayload {
    runnable_ptr: *mut (),
    location: &'static Location<'static>,
}

pub(crate) struct IosDispatcher;

impl Default for IosDispatcher {
    /// Constructs a default `IosDispatcher`.
    ///
    /// # Examples
    ///
    /// ```
    /// let dispatcher = IosDispatcher::default();
    /// ```
    fn default() -> Self {
        Self::new()
    }
}

impl IosDispatcher {
    /// Create a new iOS dispatcher.
    ///
    /// # Examples
    ///
    /// ```
    /// let _dispatcher = IosDispatcher::new();
    /// ```
    pub fn new() -> Self {
        IosDispatcher
    }
}

impl PlatformDispatcher for IosDispatcher {
    /// Capture a snapshot of all recorded thread task timings.
    ///
    /// The returned vector is produced by converting the global timing registry into a
    /// `Vec<ThreadTaskTimings>`.
    ///
    /// # Examples
    ///
    /// ```
    /// let disp = IosDispatcher::new();
    /// let all = disp.get_all_timings();
    /// // `all` is a Vec<ThreadTaskTimings> representing timings for tracked threads.
    /// assert!(all.len() >= 0);
    /// ```
    fn get_all_timings(&self) -> Vec<ThreadTaskTimings> {
        let global_timings = GLOBAL_THREAD_TIMINGS.lock();
        ThreadTaskTimings::convert(&global_timings)
    }

    /// Retrieves the timing entries recorded for the current thread.
    ///
    /// The returned vector contains all `TaskTiming` entries from the thread-local
    /// timing buffer in chronological order.
    ///
    /// # Examples
    ///
    /// ```
    /// let timings = crate::platform::ios::dispatcher::IosDispatcher::new().get_current_thread_timings();
    /// let _vec: Vec<_> = timings;
    /// ```
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

    /// Checks whether the current thread is the application's main thread.
    ///
    /// Returns `true` if the current thread is the main thread, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let disp = IosDispatcher::new();
    /// // call returns a boolean indicating main-thread status
    /// assert_eq!(disp.is_main_thread(), disp.is_main_thread());
    /// ```
    fn is_main_thread(&self) -> bool {
        let is_main_thread: BOOL = unsafe { msg_send![class!(NSThread), isMainThread] };
        is_main_thread == YES
    }

    /// Schedule a runnable to execute on a background global queue according to `priority`.
    ///
    /// The function accepts either `RunnableVariant::Meta` or `RunnableVariant::Compat`. For the
    /// Compat variant the caller location is captured and carried to the executed task for timing
    /// and diagnostics. The `priority` selects the target GCD global queue (high, default, or low).
    /// `Priority::Realtime` is not supported and treated as unreachable.
    ///
    /// # Parameters
    ///
    /// - `runnable`: The task to schedule; either a Meta runnable or a Compat runnable (whose
    ///   caller `Location` will be recorded).
    /// - `priority`: The desired scheduling priority used to choose the GCD global queue.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let dispatcher = IosDispatcher::new();
    /// // `runnable` would be constructed elsewhere as a RunnableVariant::Meta or ::Compat
    /// // dispatcher.dispatch(runnable, None, Priority::Medium);
    /// ```
    fn dispatch(&self, runnable: RunnableVariant, _label: Option<TaskLabel>, priority: Priority) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => {
                let payload = Box::new(RunnableCompatPayload {
                    runnable_ptr: runnable.into_raw().as_ptr(),
                    location: Location::caller(),
                });
                (
                    Box::into_raw(payload) as *mut c_void,
                    Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
                )
            }
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

    /// Schedules a runnable to execute on the main thread's dispatch queue.
    ///
    /// The provided `runnable` is converted into a raw context and dispatched to
    /// the main GCD queue. `RunnableVariant::Meta` runnables are dispatched directly;
    /// `RunnableVariant::Compat` runnables are wrapped in a compatibility payload that
    /// preserves the caller `Location`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let dispatcher = IosDispatcher::new();
    /// // Construct a runnable according to your application's runnable API:
    /// // let runnable = /* RunnableVariant::Meta(...) or RunnableVariant::Compat(...) */;
    /// // dispatcher.dispatch_on_main_thread(runnable, Priority::Medium);
    /// ```
    #[track_caller]
    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, _priority: Priority) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => {
                let payload = Box::new(RunnableCompatPayload {
                    runnable_ptr: runnable.into_raw().as_ptr(),
                    location: Location::caller(),
                });
                (
                    Box::into_raw(payload) as *mut c_void,
                    Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
                )
            }
        };
        unsafe {
            dispatch_async_f(dispatch_get_main_queue(), context, trampoline);
        }
    }

    /// Schedules a runnable to execute after the given duration on a high-priority global queue.
    ///
    /// The provided `runnable` is converted into a raw context and dispatched to GCD; `Compat`
    /// runnables are wrapped with a payload capturing the caller location.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::time::Duration;
    /// // Assume `dispatcher` is an `IosDispatcher` and `runnable` is a `RunnableVariant`.
    /// let dispatcher = IosDispatcher::new();
    /// // obtain or construct a `runnable` appropriate for your application
    /// // dispatcher.dispatch_after(Duration::from_millis(100), runnable);
    /// ```
    #[track_caller]
    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let (context, trampoline) = match runnable {
            RunnableVariant::Meta(runnable) => (
                runnable.into_raw().as_ptr() as *mut c_void,
                Some(trampoline as unsafe extern "C" fn(*mut c_void)),
            ),
            RunnableVariant::Compat(runnable) => {
                let payload = Box::new(RunnableCompatPayload {
                    runnable_ptr: runnable.into_raw().as_ptr(),
                    location: Location::caller(),
                });
                (
                    Box::into_raw(payload) as *mut c_void,
                    Some(trampoline_compat as unsafe extern "C" fn(*mut c_void)),
                )
            }
        };
        unsafe {
            let queue = dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0);
            let when = dispatch_time(DISPATCH_TIME_NOW, duration.as_nanos() as i64);
            dispatch_after_f(when, queue, context, trampoline);
        }
    }

    /// Spawns a new OS thread and runs the provided closure; no platform realtime scheduling is applied.
    ///
    /// The `priority` hint is ignored on iOS — the closure executes on a regular Rust thread.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::mpsc::channel;
    /// use gpui::platform::ios::dispatcher::IosDispatcher;
    ///
    /// let dispatcher = IosDispatcher::new();
    /// let (tx, rx) = channel();
    /// dispatcher.spawn_realtime(Default::default(), Box::new(move || tx.send(42).unwrap()));
    /// assert_eq!(rx.recv().unwrap(), 42);
    /// ```
    fn spawn_realtime(&self, _priority: RealtimePriority, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            f();
        });
    }
}

/// Executes a `RunnableMeta` passed as a raw pointer, recording per-task timing and honoring app liveness.
///
/// This function is used as a C-callable trampoline: it reconstructs a `Runnable::<RunnableMeta>` from
/// the provided raw pointer, checks whether the application is still alive, records a `TaskTiming`
/// (start and end) in the calling thread's timing store, runs the task, and updates the recorded end
/// time. If the app is not alive the runnable is dropped without being executed.
///
/// # Parameters
///
/// - `runnable`: a raw pointer previously produced by converting a `Runnable::<RunnableMeta>` into a
///   raw context. Must be non-null and point to a valid `Runnable::<RunnableMeta>`.
///
/// # Examples
///
/// ```no_run
/// use std::ffi::c_void;
///
/// // Example usage (illustrative): obtain a raw pointer for a runnable and invoke the trampoline.
/// // In real code, the pointer must come from a `Runnable::<RunnableMeta>::into_raw`-equivalent call.
/// let fake_ptr = Box::into_raw(Box::new(())) as *mut c_void;
/// unsafe {
///     // Calling the trampoline with a raw pointer; the pointer must actually be a RunnableMeta in real usage.
///     trampoline(fake_ptr);
/// }
/// ```
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

/// Executes a compat-mode runnable payload (created by the dispatcher), recording its start and end
/// times in the current thread's timing queue and freeing the payload.
///
/// The function consumes the boxed `RunnableCompatPayload` pointed to by `payload_ptr`,
/// pushes a `TaskTiming { location, start, end: None }` onto `THREAD_TIMINGS` unless the last
/// recorded timing has the same `location`, runs the contained `Runnable<()>`, then sets the
/// last timing's `end` to the completion time.
///
/// # Parameters
///
/// - `payload_ptr`: a pointer previously produced by `Box::into_raw` from a `RunnableCompatPayload`.
///   The function takes ownership of the payload and will free it.
///
/// # Examples
///
/// ```
/// # use std::ffi::c_void;
/// # use std::ptr::NonNull;
/// # // The following types are defined in the same crate/module as `trampoline_compat`.
/// # use crate::platform::ios::dispatcher::RunnableCompatPayload;
/// # use crate::platform::ios::dispatcher::trampoline_compat;
/// # use async_task::Runnable;
///
/// // Construct a payload and call the trampoline as the dispatcher would:
/// let dummy_runnable = Runnable::from_fn(|| ());
/// let runnable_ptr = dummy_runnable.into_raw().as_ptr();
/// // `location` would normally be a `&'static Location` captured at dispatch time.
/// let location: &'static _ = Box::leak(Box::new(crate::Location::UNKNOWN));
///
/// let boxed = Box::new(RunnableCompatPayload {
///     runnable_ptr,
///     location,
/// });
/// let raw = Box::into_raw(boxed) as *mut c_void;
///
/// // SAFETY: this mirrors how the dispatcher calls the trampoline with a valid payload pointer.
/// unsafe { trampoline_compat(raw) };
/// ```
extern "C" fn trampoline_compat(payload_ptr: *mut c_void) {
    let payload = unsafe { Box::from_raw(payload_ptr as *mut RunnableCompatPayload) };
    let location = payload.location;
    let task =
        unsafe { Runnable::<()>::from_raw(NonNull::new_unchecked(payload.runnable_ptr)) };

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