//! iOS window implementation using UIKit.
//!
//! This module provides a UIWindow with a custom UIView that hosts a CAMetalLayer
//! for GPU rendering. It handles touch input, keyboard events, and integrates
//! with the Metal renderer shared with macOS.

use super::metal_atlas::MetalAtlas;
use super::{
    events::{
        translate_key_press, translate_modifiers_changed, translate_pan_to_scroll,
        translate_touch_to_mouse, UIKeyModifierFlags,
    },
    metal_renderer::MetalRenderer,
    BoolExt, CGPoint, CGRect, CGSize, UIEdgeInsets,
};
use crate::{
    platform::PlatformInputHandler, point, px, size, AnyWindowHandle, Bounds, Capslock,
    DispatchEventResult, GpuSpecs, Modifiers, Pixels, PlatformAtlas, PlatformDisplay,
    PlatformInput, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    ScaledPixels, Scene, Size, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowParams,
};
use block::ConcreteBlock;
use futures::channel::oneshot;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Protocol, Sel, BOOL, NO, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle, UiKitDisplayHandle,
    UiKitWindowHandle,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr,
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, OnceLock},
};

use super::metal_renderer::InstanceBufferPool;
use super::IosDisplay;

const WINDOW_STATE_IVAR: &str = "windowState";

static VIEW_CLASS: OnceLock<&'static Class> = OnceLock::new();
static VIEW_CONTROLLER_CLASS: OnceLock<&'static Class> = OnceLock::new();

/// Registers the custom GPUIView and GPUIViewController Objective-C classes with the runtime if they have not already been registered.
///
/// # Examples
///
/// ```
/// // Safe to call multiple times; registration happens once.
/// gpui::platform::ios::window::ensure_classes_registered();
/// ```
fn ensure_classes_registered() {
    VIEW_CLASS.get_or_init(|| unsafe { register_view_class() });
    VIEW_CONTROLLER_CLASS.get_or_init(|| unsafe { register_view_controller_class() });
}

/// Registers and returns the GPUIView Objective-C class configured for Metal-backed rendering and input handling.
///
/// This declares a `GPUIView` subclass of `UIView`, adds an ivar to store a window state pointer,
/// overrides the view's backing layer to `CAMetalLayer`, and installs touch, keyboard, layout,
/// and `CALayerDelegate` callbacks expected by the GPUI runtime.
///
/// # Safety
///
/// This performs global Objective-C runtime mutation and must only be called from a thread allowed
/// to register Objective-C classes (typically the main thread). Call once before creating or using
/// `GPUIView` instances.
///
/// # Examples
///
/// ```
/// // Called during platform initialization (unsafe due to Objective-C runtime mutation).
/// unsafe { register_view_class(); }
/// ```
unsafe fn register_view_class() -> &'static Class {
    let superclass = class!(UIView);
    let mut decl = ClassDecl::new("GPUIView", superclass).unwrap();

    decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);

    // Layer class override for Metal
    decl.add_class_method(
        sel!(layerClass),
        layer_class as extern "C" fn(&Class, Sel) -> *const Class,
    );

    // Touch handling
    decl.add_method(
        sel!(touchesBegan:withEvent:),
        touches_began as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(touchesMoved:withEvent:),
        touches_moved as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(touchesEnded:withEvent:),
        touches_ended as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(touchesCancelled:withEvent:),
        touches_cancelled as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );

    // Keyboard handling (for hardware keyboards)
    decl.add_method(
        sel!(pressesBegan:withEvent:),
        presses_began as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(pressesEnded:withEvent:),
        presses_ended as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(pressesChanged:withEvent:),
        presses_changed as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );
    decl.add_method(
        sel!(pressesCancelled:withEvent:),
        presses_cancelled as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
    );

    // Make the view the first responder to receive keyboard events
    decl.add_method(
        sel!(canBecomeFirstResponder),
        can_become_first_responder as extern "C" fn(&Object, Sel) -> BOOL,
    );

    // Layout
    decl.add_method(
        sel!(layoutSubviews),
        layout_subviews as extern "C" fn(&Object, Sel),
    );

    // CALayerDelegate
    decl.add_protocol(Protocol::get("CALayerDelegate").unwrap());
    decl.add_method(
        sel!(displayLayer:),
        display_layer as extern "C" fn(&Object, Sel, *mut Object),
    );

    decl.register()
}

/// Register a custom UIViewController for the GPUI view.
unsafe fn register_view_controller_class() -> &'static Class {
    let superclass = class!(UIViewController);
    let mut decl = ClassDecl::new("GPUIViewController", superclass).unwrap();

    decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);

    // View lifecycle
    decl.add_method(
        sel!(viewDidLoad),
        view_did_load as extern "C" fn(&Object, Sel),
    );
    decl.add_method(
        sel!(viewWillTransitionToSize:withTransitionCoordinator:),
        view_will_transition as extern "C" fn(&Object, Sel, CGSize, *mut Object),
    );
    decl.add_method(
        sel!(viewSafeAreaInsetsDidChange),
        safe_area_insets_did_change as extern "C" fn(&Object, Sel),
    );

    // Trait collection changes (dark mode, etc.)
    decl.add_method(
        sel!(traitCollectionDidChange:),
        trait_collection_did_change as extern "C" fn(&Object, Sel, *mut Object),
    );

    decl.register()
}

// Objective-C callback implementations

/// Specify `CAMetalLayer` as the backing Core Animation layer for the custom view.
///
/// This function is intended to be used as an Objective-C `+layerClass` override so the view
/// is backed by a `CAMetalLayer`, enabling Metal-backed rendering.
///
/// # Examples
///
/// ```
/// // Demonstrates the function returning the CAMetalLayer class pointer.
/// let ui_view = unsafe { &*class!(UIView) };
/// let layer_cls = unsafe { layer_class(ui_view, sel!(layerClass)) };
/// assert_eq!(layer_cls, class!(CAMetalLayer));
/// ```
extern "C" fn layer_class(_this: &Class, _sel: Sel) -> *const Class {
    class!(CAMetalLayer)
}

/// Allows the view to become the first responder so it can receive keyboard events.
///
/// This Objective-C callback always permits the view to become first responder by returning `YES`.
///
/// # Returns
///
/// `YES` if the view can become first responder, `NO` otherwise.
///
/// # Examples
///
/// ```rust,ignore
/// // Used as an Objective-C method implementation for `-canBecomeFirstResponder`.
/// let res = unsafe { can_become_first_responder(obj, sel) };
/// assert_eq!(res, YES);
/// ```
extern "C" fn can_become_first_responder(_this: &Object, _sel: Sel) -> BOOL {
    YES
}

/// Objective-C callback invoked when one or more touches begin on the view.
///
/// Forwards the provided touches to the internal touch handler with the `"began"` phase.
///
/// # Examples
///
/// ```no_run
/// use std::ptr;
///
/// // SAFETY: example only; in real usage these are Objective-C pointers supplied by UIKit.
/// unsafe {
///     touches_began(ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
/// }
/// ```
extern "C" fn touches_began(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "began");
}

/// Handles UIKit "touches moved" events for the view by dispatching moved-touch input to the window state.
///
/// # Examples
///
/// ```ignore
/// // Called by the Objective-C runtime when touches move on the view.
/// // Signature: extern "C" fn(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object)
/// touches_moved(this, sel, touches, event);
/// ```
extern "C" fn touches_moved(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "moved");
}

/// Objective‑C selector called when a touch sequence ends; forwards the touches to the internal touch handler with phase "ended".
///
/// This function is installed as the `touchesEnded:withEvent:` Objective‑C callback on the custom UIView and delegates processing to `handle_touches`.
///
/// # Examples
///
/// ```
/// // Safe example showing how the extern function can be invoked from Rust code.
/// // In real usage UIKit will call this function; here we call it with null pointers to ensure it can be referenced.
///
/// use std::ptr;
/// extern "C" {
///     fn touches_ended(this: *const (), sel: *const (), touches: *mut (), event: *mut ());
/// }
///
/// unsafe {
///     // Invocation with null pointers — only for demonstration; real calls come from ObjC runtime.
///     let _ = touches_ended(ptr::null(), ptr::null(), ptr::null_mut(), ptr::null_mut());
/// }
/// ```
extern "C" fn touches_ended(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "ended");
}

/// Called by UIKit when a touch sequence is cancelled and forwards the cancelled touches to the window's touch handling pipeline.
///
/// # Safety
/// This function is an Objective-C callback; the `this`, `touches`, and `event` parameters must be valid Objective-C objects (or null) provided by the runtime.
///
/// # Examples
///
/// ```
/// // Typically invoked by the Objective-C runtime. Can be called directly for testing:
/// unsafe { touches_cancelled(&*(objc::runtime::NSObject::class() as *const _ as *const objc::runtime::Object), std::mem::transmute(0usize), std::ptr::null_mut(), std::ptr::null_mut()); }
/// ```
extern "C" fn touches_cancelled(
    this: &Object,
    _sel: Sel,
    touches: *mut Object,
    _event: *mut Object,
) {
    handle_touches(this, touches, "cancelled");
}

/// Translates a UIKit touch set into mouse input events and dispatches them to the window's input callback.
///
/// Iterates the provided `touches` collection, converts each touch into a platform `Mouse` event
/// using the view's current modifier state, and forwards any resulting events to the window state
/// input handler. If the view has no associated window state, the function returns without action.
///
/// # Parameters
///
/// - `view`: The Objective-C view that received the touches; used as the target for coordinate translation and to look up the associated window state.
/// - `touches`: An Objective-C `NSSet`/collection of `UITouch` objects to process.
/// - `_phase`: The touch phase string (unused by this implementation).
///
/// # Examples
///
/// ```no_run
/// // Called from Objective-C callbacks; shown here for illustration only.
/// // `view` and `touches` are Objective-C objects provided by UIKit.
/// unsafe {
///     handle_touches(&*view_obj, touches_obj, "began");
/// }
/// ```
fn handle_touches(view: &Object, touches: *mut Object, _phase: &str) {
    unsafe {
        let state = get_window_state(view);
        if state.is_null() {
            return;
        }
        let state = &*state;

        let count: usize = msg_send![touches, count];
        let all_objects: *mut Object = msg_send![touches, allObjects];

        for i in 0..count {
            let touch: *mut Object = msg_send![all_objects, objectAtIndex: i];

            if let Some(event) = translate_touch_to_mouse(
                touch,
                view as *const Object as *mut Object,
                state.modifiers.lock().clone(),
            ) {
                dispatch_event(&state, event);
            }
        }
    }
}

/// Objective-C callback for `pressesBegan:withEvent:`.
///
/// Forwards the received presses to the internal press handler indicating the presses began.
///
/// # Examples
///
/// ```no_run
/// use std::ptr;
///
/// // This callback is invoked by the Objective-C runtime. It can also be called directly
/// // for testing purposes (unsafe, runtime-dependent).
/// unsafe {
///     presses_began(ptr::null(), std::ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
/// }
/// ```
extern "C" fn presses_began(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    handle_presses(this, presses, true);
}

/// Objective-C callback invoked when one or more physical key presses end on the view.
///
/// This function is called by UIKit when presses end and notifies the window layer of the ended presses.
///
/// # Parameters
///
/// - `this`: The Objective-C receiver (the view or view controller instance).
/// - `presses`: Pointer to an Objective-C collection containing the ended `UIPress` objects (typically an `NSSet` or `NSArray`).
/// - `_event`: The associated event object (ignored by this handler).
///
/// # Examples
///
/// ```
/// # use objc::runtime::{Object, Sel};
/// unsafe {
///     // Construct a zeroed Object and Sel for demonstration; UIKit supplies real values at runtime.
///     let obj: Object = std::mem::zeroed();
///     let sel: Sel = std::mem::zeroed();
///     let presses: *mut Object = std::ptr::null_mut();
///     presses_ended(&obj, sel, presses, std::ptr::null_mut());
/// }
/// ```
extern "C" fn presses_ended(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    handle_presses(this, presses, false);
}

/// Objective-C callback invoked when hardware key presses change (pressure-sensitive).
///
/// Routes pressure-sensitive press-change events to the internal press handler and treats them as key-down events.
///
/// # Parameters
///
/// - `this`: Objective-C receiver (the view instance).
/// - `presses`: pointer to an Objective-C collection of `UIPress` objects (may be null).
/// - `_event`: unused UIEvent pointer.
///
/// # Examples
///
/// ```
/// // Typically registered with Objective-C runtime; shown here as a synthetic call.
/// use std::ptr;
/// extern "C" { fn presses_changed(this: *const (), sel: *const (), presses: *mut (), event: *mut ()); }
/// // Safe to call with nulls for testing registration wiring (no-op in this context).
/// unsafe { presses_changed(ptr::null(), ptr::null(), ptr::null_mut(), ptr::null_mut()); }
/// ```
extern "C" fn presses_changed(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    // Changed is used for pressure-sensitive keys, treat as key down
    handle_presses(this, presses, true);
}

/// Handles cancelled hardware key press events from UIKit.
///
/// Invoked as the Objective-C selector for `pressesCancelled:withEvent:`; forwards the provided presses set to the window's press handler so cancelled presses are processed accordingly.
///
/// # Examples
///
/// ```
/// // Illustrative, unsafe call only — real usage is driven by the Objective-C runtime.
/// unsafe {
///     // `this`, `presses`, and `event` are Objective-C objects in real usage.
///     crate::platform::ios::window::presses_cancelled(std::ptr::null(), 0usize as _, std::ptr::null_mut(), std::ptr::null_mut());
/// }
/// ```
extern "C" fn presses_cancelled(
    this: &Object,
    _sel: Sel,
    presses: *mut Object,
    _event: *mut Object,
) {
    handle_presses(this, presses, false);
}

/// Handle hardware keyboard presses for the view's associated window state.
///
/// Updates the stored modifier keys when a hardware key's modifier flags change,
/// dispatches a `ModifiersChangedEvent` when modifiers differ from the previous state,
/// and translates each press into a key event which is dispatched to the window's input callback.
/// Safe to call from Objective-C runtime callbacks; no action is taken if the view has no associated window state.
///
/// # Parameters
///
/// - `view`: Objective-C `UIView` instance that carries the window state ivar.
/// - `presses`: Objective-C `NSSet` of `UIPress` objects representing the current press events.
/// - `is_key_down`: `true` if the incoming presses represent key-down events, `false` for key-up.
///
/// # Examples
///
/// ```ignore
/// // Called from Objective-C runtime; demonstrates intended usage pattern.
/// unsafe {
///     // `view` and `presses` are provided by UIKit in real callbacks.
///     handle_presses(view, presses, true);
/// }
/// ```
fn handle_presses(view: &Object, presses: *mut Object, is_key_down: bool) {
    unsafe {
        let state = get_window_state(view);
        if state.is_null() {
            return;
        }
        let state = &*state;

        let count: usize = msg_send![presses, count];
        let all_objects: *mut Object = msg_send![presses, allObjects];

        for i in 0..count {
            let press: *mut Object = msg_send![all_objects, objectAtIndex: i];

            // Check if this is a repeat
            let key: *mut Object = msg_send![press, key];
            let is_repeat: bool = if !key.is_null() {
                msg_send![press, isRepeating]
            } else {
                false
            };

            // Update modifier state from hardware keyboard and dispatch ModifiersChangedEvent if changed
            if !key.is_null() {
                let modifier_flags: i64 = msg_send![key, modifierFlags];
                let new_modifiers = UIKeyModifierFlags(modifier_flags).to_modifiers();
                let old_modifiers = state.modifiers.lock().clone();
                if new_modifiers != old_modifiers {
                    *state.modifiers.lock() = new_modifiers;
                    let modifiers_event = translate_modifiers_changed(modifier_flags);
                    dispatch_event(state, modifiers_event);
                }
            }

            if let Some(event) = translate_key_press(press, is_key_down, is_repeat) {
                dispatch_event(state, event);
            }
        }
    }
}

/// Handles layout updates for the custom UIView.
///
/// Updates the view's backing CAMetalLayer drawable size and contents scale to
/// match the view's bounds and content scale factor, then invokes the window's
/// registered resize callback (if any) with the new size in pixels and the
/// current scale factor. If no WindowState is associated with the view, this
/// function returns without side effects.
extern "C" fn layout_subviews(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIView);
        let _: () = msg_send![super(this, superclass), layoutSubviews];

        let state = get_window_state(this);
        if state.is_null() {
            return;
        }
        let state = &*state;

        // Update Metal layer size
        let bounds: CGRect = msg_send![this, bounds];
        let scale: f64 = msg_send![this, contentScaleFactor];

        let layer: *mut Object = msg_send![this, layer];
        let drawable_size = CGSize {
            width: bounds.size.width * scale,
            height: bounds.size.height * scale,
        };
        let _: () = msg_send![layer, setDrawableSize: drawable_size];
        let _: () = msg_send![layer, setContentsScale: scale];

        // Notify of resize
        if let Some(callback) = state.resize_callback.lock().as_mut() {
            callback(
                size(px(bounds.size.width as f32), px(bounds.size.height as f32)),
                scale as f32,
            );
        }
    }
}

/// Requests a render frame that must be presented for the window associated with the Objective-C view.
///
/// This is an Objective-C callback for `-displayLayer:`; it looks up the Rust `WindowState` stored on
/// `this` and invokes the stored `request_frame_callback` with `require_presentation = true`.
///
/// # Parameters
///
/// - `this`: Objective-C view or controller receiving the `displayLayer:` message; used to retrieve the associated window state.
/// - `_layer`: the CALayer being displayed (unused).
extern "C" fn display_layer(this: &Object, _sel: Sel, _layer: *mut Object) {
    unsafe {
        let state = get_window_state(this);
        if state.is_null() {
            return;
        }
        let state = &*state;

        if let Some(callback) = state.request_frame_callback.lock().as_mut() {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render: false,
            });
        }
    }
}

/// Forwards the Objective-C `-viewDidLoad` message to the superclass.
///
/// This function is intended to be used as the Objective-C implementation for `viewDidLoad`; it calls the superclass
/// (`UIViewController`) implementation.
///
/// # Examples
///
/// ```no_run
/// use objc::runtime::{Object, Sel};
/// // `this` and `sel` are provided by the Objective-C runtime; the following demonstrates calling the function
/// // in an unsafe context and should not be executed as-is.
/// let this: &Object = unsafe { &*(0 as *const Object) };
/// let sel: Sel = unsafe { std::mem::zeroed() };
/// unsafe { crate::view_did_load(this, sel) };
/// ```
extern "C" fn view_did_load(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewDidLoad];
    }
}

/// Notify the window's resize callback when the view controller is about to transition to a new size.
///
/// This Objective‑C selector handler is invoked by UIKit when a view controller will transition
/// to `new_size`. After forwarding the message to `super`, it retrieves the associated
/// `WindowState` and, if a resize callback is registered, calls it with the new size converted
/// to pixel units and the current scale factor.
///
/// `new_size` is provided in UIKit points; the callback receives a `Size<Pixels>` and an `f32`
/// scale factor. The `coordinator` parameter is the UIKit transition coordinator (forwarded to
/// `super` but not otherwise inspected here).
///
/// # Examples
///
/// ```no_run
/// // This function is invoked by the Objective-C runtime; example shows the callback shape.
/// // (Illustrative only — the actual call happens from UIKit.)
/// let new_size_points = CGSize { width: 320.0, height: 480.0 };
/// // Registered callback signature: FnMut(Size<Pixels>, f32)
/// // When invoked, it will receive `size(px(320.0), px(480.0))` and the current scale factor.
/// ```
extern "C" fn view_will_transition(
    this: &Object,
    _sel: Sel,
    new_size: CGSize,
    coordinator: *mut Object,
) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewWillTransitionToSize: new_size withTransitionCoordinator: coordinator];

        // Handle rotation/resize
        let state_ptr: *mut c_void = *this.get_ivar(WINDOW_STATE_IVAR);
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const WindowState);
            if let Some(callback) = state.resize_callback.lock().as_mut() {
                let scale = state.scale_factor.lock().clone();
                callback(
                    size(px(new_size.width as f32), px(new_size.height as f32)),
                    scale,
                );
            }
        }
    }
}

/// Handles a UIViewController safe-area change by treating it as a layout/resize event.
///
/// Calls the superclass implementation, looks up the associated `WindowState` stored on the
/// Objective-C controller, and if a resize callback is registered, obtains the controller's
/// view bounds, converts them to pixel coordinates using the state's scale factor, and invokes
/// the callback with the new size and scale.
///
/// # Examples
///
/// ```no_run
/// // This demonstrates invoking the callback wiring; in practice UIKit calls this selector.
/// unsafe {
///     // `vc` is a pointer to an Objective-C UIViewController instance.
///     // let vc: &Object = ...;
///     // safe_area_insets_did_change(vc, Sel::register("viewSafeAreaInsetsDidChange"));
/// }
/// ```
extern "C" fn safe_area_insets_did_change(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewSafeAreaInsetsDidChange];

        // Notify about safe area changes by treating them as a layout/resize event.
        let state_ptr: *mut c_void = *this.get_ivar(WINDOW_STATE_IVAR);
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const WindowState);

            if let Some(callback) = state.resize_callback.lock().as_mut() {
                // Obtain the current view size in points and convert to pixels.
                let view: *mut Object = msg_send![this, view];
                if !view.is_null() {
                    let bounds: CGRect = msg_send![view, bounds];
                    let size = bounds.size;
                    let scale = state.scale_factor.lock().clone();
                    callback(size(px(size.width as f32), px(size.height as f32)), scale);
                }
            }
        }

        // Could notify about safe area changes here
    }
}

/// Called by Objective-C when a view controller's trait collection changes (e.g., dark/light appearance).
///
/// This Objective-C callback forwards the message to the superclass implementation and, if the view's
/// associated `WindowState` contains an `appearance_changed_callback`, invokes that callback.
///
/// # Examples
///
/// ```rust
/// # no_run
/// // Registered as an Objective-C method implementation; UIKit will call this when traits change.
/// // The function will call the superclass implementation and then run the stored appearance callback.
/// // Example usage is performed by the ObjC runtime when traitCollectionDidChange: occurs.
/// unsafe {
///     // objc runtime will invoke trait_collection_did_change(this, sel, previous);
/// }
/// ```
extern "C" fn trait_collection_did_change(this: &Object, _sel: Sel, previous: *mut Object) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), traitCollectionDidChange: previous];

        let state_ptr: *mut c_void = *this.get_ivar(WINDOW_STATE_IVAR);
        if !state_ptr.is_null() {
            let state = &*(state_ptr as *const WindowState);
            if let Some(callback) = state.appearance_changed_callback.lock().as_mut() {
                callback();
            }
        }
    }
}

/// Retrieves the raw pointer to the WindowState stored in the Objective-C ivar of a view.
///
/// The provided `view` is expected to be an Objective-C object that contains the `WINDOW_STATE_IVAR` ivar
/// holding a pointer to a `WindowState`. The returned pointer may be null; callers must check for null
/// before dereferencing.
///
/// # Safety
///
/// - The function is unsafe because it reads an ivar from an Objective-C object and returns a raw pointer.
/// - The caller must ensure the `view` is a valid Objective-C object with the expected ivar layout.
/// - The caller must check the returned pointer for null and uphold any aliasing and lifetime requirements
///   before converting it to a reference.
///
/// # Returns
///
/// A raw pointer to the `WindowState` stored in the view's ivar, or a null pointer if none is set.
///
/// # Examples
///
/// ```
/// // SAFETY: `view` must be a valid Objective-C object with the WINDOW_STATE_IVAR.
/// let state_ptr = unsafe { get_window_state(&view) };
/// if !state_ptr.is_null() {
///     // SAFETY: We've checked for null; ensure no concurrent mutable aliasing before using.
///     let state: &WindowState = unsafe { &*state_ptr };
///     // use `state`...
/// }
/// ```
unsafe fn get_window_state(view: &Object) -> *const WindowState {
    // May be null; callers must check the returned pointer before dereferencing.
    let state_ptr: *mut c_void = *view.get_ivar(WINDOW_STATE_IVAR);
    state_ptr as *const WindowState
}
/// Forwards a platform input event to the window state's registered input callback, if any.
///
/// If the window state has an input callback set, the event is passed to that callback; otherwise the event is dropped.
///
/// # Examples
///
/// ```
/// // Example usage (illustrative):
/// // let state: WindowState = ...;
/// // dispatch_event(&state, PlatformInput::MouseMove { x: 10.0, y: 20.0 });
/// ```
fn dispatch_event(state: &WindowState, event: PlatformInput) {
    if let Some(callback) = state.input_callback.lock().as_mut() {
        callback(event);
    }
}

/// Internal window state shared between Rust and Objective-C callbacks.
struct WindowState {
    handle: AnyWindowHandle,
    renderer: Mutex<MetalRenderer>,
    input_callback: Mutex<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    request_frame_callback: Mutex<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    resize_callback: Mutex<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    moved_callback: Mutex<Option<Box<dyn FnMut()>>>,
    active_status_callback: Mutex<Option<Box<dyn FnMut(bool)>>>,
    hover_status_callback: Mutex<Option<Box<dyn FnMut(bool)>>>,
    should_close_callback: Mutex<Option<Box<dyn FnMut() -> bool>>>,
    close_callback: Mutex<Option<Box<dyn FnOnce()>>>,
    appearance_changed_callback: Mutex<Option<Box<dyn FnMut()>>>,
    hit_test_callback: Mutex<Option<Box<dyn FnMut() -> Option<WindowControlArea>>>>,
    input_handler: Mutex<Option<PlatformInputHandler>>,
    modifiers: Mutex<Modifiers>,
    scale_factor: Mutex<f32>,
}

/// iOS window implementation.
pub struct IosWindow {
    ui_window: *mut Object,
    view: *mut Object,
    view_controller: *mut Object,
    state: Arc<WindowState>,
}

unsafe impl Send for IosWindow {}

impl IosWindow {
    /// Create a new iOS window backed by UIKit with a CAMetalLayer-backed Metal renderer.
    ///
    /// This constructs and configures a UIWindow, a custom UIViewController and UIView, attaches
    /// a Metal renderer and its CAMetalLayer to the view, initializes shared window state
    /// (callbacks, input handler, modifiers, and scale factor), and makes the window key and visible.
    /// The `_params` argument is unused on iOS.
    ///
    /// # Parameters
    ///
    /// - `handle`: Platform-agnostic window handle to associate with the created window.
    /// - `renderer_context`: Shared renderer context used to construct the Metal renderer.
    ///
    /// # Returns
    ///
    /// An initialized `IosWindow` on success, or an `anyhow::Error` on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::{Arc, Mutex};
    /// // `AnyWindowHandle`, `WindowParams`, and `InstanceBufferPool` are provided by the crate.
    /// let handle = AnyWindowHandle::default();
    /// let params = WindowParams::default();
    /// let renderer_ctx = Arc::new(Mutex::new(InstanceBufferPool::new()));
    /// let window = IosWindow::new(handle, params, renderer_ctx).unwrap();
    /// ```
    pub fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        renderer_context: Arc<Mutex<InstanceBufferPool>>,
    ) -> anyhow::Result<Self> {
        ensure_classes_registered();

        unsafe {
            // Get the main screen
            let screen: *mut Object = msg_send![class!(UIScreen), mainScreen];
            let screen_bounds: CGRect = msg_send![screen, bounds];
            let scale: f64 = msg_send![screen, scale];

            // Create the UIWindow
            let ui_window: *mut Object = msg_send![class!(UIWindow), alloc];
            let ui_window: *mut Object = msg_send![ui_window, initWithFrame: screen_bounds];

            // Create the view controller
            let view_controller_class = *VIEW_CONTROLLER_CLASS.get().unwrap();
            let view_controller: *mut Object = msg_send![view_controller_class, alloc];
            let view_controller: *mut Object = msg_send![view_controller, init];

            // Create the custom view
            let view_class = *VIEW_CLASS.get().unwrap();
            let view: *mut Object = msg_send![view_class, alloc];
            let view: *mut Object = msg_send![view, initWithFrame: screen_bounds];

            // Create the renderer - this creates its own CAMetalLayer
            let renderer = MetalRenderer::new(renderer_context);

            // Get the Metal layer from the renderer and set it as the view's layer
            let metal_layer = renderer.layer_ptr();
            let _: () = msg_send![view, setLayer: metal_layer];

            // Configure the layer scale
            let layer: *mut Object = msg_send![view, layer];
            let _: () = msg_send![layer, setContentsScale: scale];
            let drawable_size = CGSize {
                width: screen_bounds.size.width * scale,
                height: screen_bounds.size.height * scale,
            };
            let _: () = msg_send![layer, setDrawableSize: drawable_size];

            // Create the window state
            let state = Arc::new(WindowState {
                handle,
                renderer: Mutex::new(renderer),
                input_callback: Mutex::new(None),
                request_frame_callback: Mutex::new(None),
                resize_callback: Mutex::new(None),
                moved_callback: Mutex::new(None),
                active_status_callback: Mutex::new(None),
                hover_status_callback: Mutex::new(None),
                should_close_callback: Mutex::new(None),
                close_callback: Mutex::new(None),
                appearance_changed_callback: Mutex::new(None),
                hit_test_callback: Mutex::new(None),
                input_handler: Mutex::new(None),
                modifiers: Mutex::new(Modifiers::default()),
                scale_factor: Mutex::new(scale as f32),
            });

            // Store state pointer in Objective-C objects
            let state_ptr = Arc::as_ptr(&state) as *mut c_void;
            (*view).set_ivar(WINDOW_STATE_IVAR, state_ptr);
            (*view_controller).set_ivar(WINDOW_STATE_IVAR, state_ptr);

            // Keep the Arc alive - increment for each ivar that holds the pointer
            Arc::increment_strong_count(Arc::as_ptr(&state));
            Arc::increment_strong_count(Arc::as_ptr(&state));
            // Set up the view hierarchy
            let _: () = msg_send![view_controller, setView: view];
            let _: () = msg_send![ui_window, setRootViewController: view_controller];

            // Make the view first responder to receive keyboard events
            let _: () = msg_send![view, becomeFirstResponder];

            // Make the window visible
            let _: () = msg_send![ui_window, makeKeyAndVisible];

            Ok(Self {
                ui_window,
                view,
                view_controller,
                state,
            })
        }
    }
}

impl HasWindowHandle for IosWindow {
    /// Constructs a `raw_window_handle::WindowHandle` that references this window's UIKit view and view controller.
    ///
    /// Creates a `RawWindowHandle::UiKit` backed by the view pointer and sets the corresponding view controller pointer.
    ///
    /// # Returns
    ///
    /// `Ok(WindowHandle)` containing a borrowed `RawWindowHandle::UiKit` referencing the view and view controller, or
    /// an `Err(HandleError)` if producing the handle fails.
    ///
    /// # Examples
    ///
    /// ```
    /// // `win` is an `IosWindow`
    /// let handle = win.window_handle().unwrap();
    /// ```
    fn window_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
    {
        // UiKitWindowHandle::new takes the ui_view (not ui_window)
        let mut handle = UiKitWindowHandle::new(NonNull::new(self.view as *mut c_void).unwrap());
        handle.ui_view_controller = NonNull::new(self.view_controller as *mut c_void);

        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(RawWindowHandle::UiKit(handle)) })
    }
}

impl HasDisplayHandle for IosWindow {
    /// Construct a raw display handle for the iOS (UIKit) main display.
    ///
    /// # Examples
    ///
    /// ```
    /// use raw_window_handle::{RawDisplayHandle, UiKitDisplayHandle, DisplayHandle};
    ///
    /// // Create a borrowed DisplayHandle wrapping a UiKit RawDisplayHandle
    /// let handle = DisplayHandle::borrow_raw(RawDisplayHandle::UiKit(UiKitDisplayHandle::new()));
    /// // `handle` can be passed to APIs that accept a `DisplayHandle<'_>`
    /// ```
    ///
    /// # Returns
    ///
    /// `Ok(DisplayHandle)` containing a borrowed `RawDisplayHandle::UiKit` for the main iOS display, or an `Err(HandleError)` if acquiring the handle fails.
    fn display_handle(
        &self,
    ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
    {
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(RawDisplayHandle::UiKit(
                UiKitDisplayHandle::new(),
            ))
        })
    }
}

impl PlatformWindow for IosWindow {
    /// Compute the window's bounds in physical pixels.
    ///
    /// The returned bounds use the view's frame (origin and size) multiplied by the current
    /// scale factor to produce pixel-aligned coordinates.
    ///
    /// # Returns
    ///
    /// `Bounds<Pixels>` representing the view's origin and size in physical pixels.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an instance of `IosWindow`
    /// let pixel_bounds = window.bounds();
    /// println!("Window pixel size: {:?}", pixel_bounds.size);
    /// ```
    fn bounds(&self) -> Bounds<Pixels> {
        unsafe {
            let frame: CGRect = msg_send![self.view, frame];
            let scale = self.scale_factor();
            Bounds {
                origin: point(
                    px(frame.origin.x as f32 * scale),
                    px(frame.origin.y as f32 * scale),
                ),
                size: size(
                    px(frame.size.width as f32 * scale),
                    px(frame.size.height as f32 * scale),
                ),
            }
        }
    }

    /// Report whether the window is considered maximized on iOS.
    ///
    /// On iOS, windows are treated as always maximized.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an `IosWindow` instance
    /// assert!(window.is_maximized());
    /// ```
    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    /// Provide the window's bounds wrapped as a fullscreen `WindowBounds`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given an `IosWindow` instance `w`, this returns fullscreen bounds:
    /// // let bounds = w.window_bounds();
    /// ```
    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    /// Compute the content size in pixels, accounting for the view's safe area insets and the current scale factor.
    ///
    /// The returned size is (view bounds minus safe area insets) multiplied by the window's scale factor, expressed in `Pixels`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an initialized `IosWindow`
    /// let size = window.content_size();
    /// println!("content size: {:?}", size);
    /// ```
    fn content_size(&self) -> Size<Pixels> {
        unsafe {
            let bounds: CGRect = msg_send![self.view, bounds];
            let insets: UIEdgeInsets = msg_send![self.view, safeAreaInsets];
            let scale = self.scale_factor();

            size(
                px((bounds.size.width - insets.left - insets.right) as f32 * scale),
                px((bounds.size.height - insets.top - insets.bottom) as f32 * scale),
            )
        }
    }

    /// Returns the window's current content scale factor.
    ///
    /// The scale factor represents the number of physical pixels per logical point.
    ///
    /// # Examples
    ///
    /// ```
    /// // `window` is an instance with a valid state
    /// let scale = window.scale_factor();
    /// assert!(scale >= 1.0);
    /// ```
    fn scale_factor(&self) -> f32 {
        *self.state.scale_factor.lock()
    }

    /// Determine the window's current appearance from its view's trait collection.
    ///
    /// # Returns
    /// `WindowAppearance::Dark` if the view's trait collection indicates dark user interface style, `WindowAppearance::Light` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an `IosWindow`
    /// let appearance = window.appearance();
    /// match appearance {
    ///     WindowAppearance::Dark => println!("Dark mode"),
    ///     WindowAppearance::Light => println!("Light mode"),
    /// }
    /// ```
    fn appearance(&self) -> WindowAppearance {
        unsafe {
            let trait_collection: *mut Object = msg_send![self.view, traitCollection];
            let style: i64 = msg_send![trait_collection, userInterfaceStyle];

            match style {
                2 => WindowAppearance::Dark,
                _ => WindowAppearance::Light,
            }
        }
    }

    /// Get the platform display associated with this window.
    ///
    /// # Examples
    ///
    /// ```
    /// let display = window.display();
    /// assert!(display.is_some());
    /// ```
    fn
    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    /// Provides a default mouse position for the window.
    ///
    /// This returns a fallback position because iOS does not maintain a persistent mouse cursor.
    /// The chosen fallback is the center point of the window's bounds in pixels.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Obtain a window instance from your platform setup and query its mouse position.
    /// let pos = window.mouse_position();
    /// println!("Mouse fallback position: {:?}", pos);
    /// ```
    fn
    fn mouse_position(&self) -> Point<Pixels> {
        // iOS doesn't have a persistent mouse position
        // Return center of the view as a reasonable default
        let bounds = self.bounds();
        point(bounds.size.width / 2.0, bounds.size.height / 2.0)
    }

    /// Returns the current keyboard modifier state.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an `IosWindow` instance.
    /// let mods = window.modifiers();
    /// // `mods` is a `Modifiers` value describing active modifier keys (Shift, Ctrl, Alt, etc.).
    /// ```
    fn modifiers(&self) -> Modifiers {
        self.state.modifiers.lock().clone()
    }

    /// Reports the current Caps Lock state for this window.
    ///
    /// # Examples
    ///
    /// ```
    /// // Obtain an `IosWindow` named `window` in your application context.
    /// // Here we only demonstrate the call; replace with a real window in tests.
    /// // let window: IosWindow = ...;
    /// // let caps = window.capslock();
    /// // assert!(matches!(caps, Capslock::On | Capslock::Off));
    /// ```
    ///
    /// # Returns
    /// The current `Capslock` state (`Capslock::On` if active, `Capslock::Off` otherwise).
    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    /// Set the platform input handler that will receive translated input events.
    ///
    /// Calling this replaces any previously installed input handler.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use gpui::platform::PlatformInputHandler;
    /// # use gpui::platform::DispatchEventResult;
    /// # // `window` is an existing `IosWindow`
    /// let handler: PlatformInputHandler = Box::new(|_event| DispatchEventResult::NotHandled);
    /// // window.set_input_handler(handler);
    /// ```
    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.state.input_handler.lock() = Some(input_handler);
    }

    /// Take ownership of the current platform input handler, leaving `None` in its place.
    ///
    /// # Returns
    ///
    /// `Some(handler)` if a handler was set, `None` otherwise.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let mut window = get_ios_window(); // obtain an IosWindow
    /// let handler = window.take_input_handler();
    /// // `handler` now holds the previously registered PlatformInputHandler, if any.
    /// ```
    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.state.input_handler.lock().take()
    }

    /// Present a modal alert with the given message, detail, and answer buttons, and return a receiver that yields the index of the selected answer.
    ///
    /// The returned receiver yields the index (0-based) of the button the user tapped.
    /// If the prompt could not be presented, `None` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// // Example usage (platform-specific; not executable in a non-iOS environment)
    /// let answers = [PromptButton::new("OK"), PromptButton::new("Cancel")];
    /// if let Some(rx) = window.prompt(PromptLevel::Normal, "Confirm", None, &answers) {
    ///     // `rx` yields the index of the selected answer (0 for "OK", 1 for "Cancel").
    ///     // In real code, await or poll the receiver to obtain the result.
    /// }
    /// ```
    fn prompt(
        &self,
        level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // UIAlertControllerStyle constants
        const UI_ALERT_CONTROLLER_STYLE_ALERT: isize = 1;

        // UIAlertActionStyle constants
        const UI_ALERT_ACTION_STYLE_DEFAULT: isize = 0;
        const UI_ALERT_ACTION_STYLE_CANCEL: isize = 1;
        const UI_ALERT_ACTION_STYLE_DESTRUCTIVE: isize = 2;

        // Create CStrings for title and message (must be null-terminated for Objective-C)
        let msg_cstring = std::ffi::CString::new(msg).unwrap_or_default();
        let detail_cstring = detail.and_then(|d| std::ffi::CString::new(d).ok());

        // Create UIAlertController
        let alert_controller: *mut Object = unsafe {
            let title_ns: *mut Object =
                msg_send![class!(NSString), stringWithUTF8String: msg_cstring.as_ptr()];
            let message_ns: *mut Object = match &detail_cstring {
                Some(cstring) => {
                    msg_send![class!(NSString), stringWithUTF8String: cstring.as_ptr()]
                }
                None => ptr::null_mut(),
            };

            msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: title_ns
                message: message_ns
                preferredStyle: UI_ALERT_CONTROLLER_STYLE_ALERT
            ]
        };

        let (done_tx, done_rx) = oneshot::channel();
        let done_tx = Rc::new(Cell::new(Some(done_tx)));

        // Add actions for each answer
        for (index, answer) in answers.iter().enumerate() {
            let action_style = match level {
                PromptLevel::Critical if index == 0 => UI_ALERT_ACTION_STYLE_DESTRUCTIVE,
                _ if answer.is_cancel() => UI_ALERT_ACTION_STYLE_CANCEL,
                _ => UI_ALERT_ACTION_STYLE_DEFAULT,
            };

            let label_cstring = std::ffi::CString::new(answer.label()).unwrap_or_default();

            let done_tx_clone = done_tx.clone();
            let block = ConcreteBlock::new(move |_action: *mut Object| {
                if let Some(tx) = done_tx_clone.take() {
                    let _ = tx.send(index);
                }
            });
            let block = block.copy();

            unsafe {
                let label_ns: *mut Object =
                    msg_send![class!(NSString), stringWithUTF8String: label_cstring.as_ptr()];

                let action: *mut Object = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: label_ns
                    style: action_style
                    handler: &*block
                ];

                let _: () = msg_send![alert_controller, addAction: action];
            }
        }

        // Present the alert controller from the view controller
        unsafe {
            let _: () = msg_send![
                self.view_controller,
                presentViewController: alert_controller
                animated: YES
                completion: ptr::null::<c_void>()
            ];
        }

        Some(done_rx)
    }

    /// Makes the underlying UIKit window key and visible.
    ///
    /// This gives the window keyboard focus and presents it on screen.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // assuming `window` is an `IosWindow`
    /// window.activate();
    /// ```
    fn activate(&self) {
        unsafe {
            let _: () = msg_send![self.ui_window, makeKeyAndVisible];
        }
    }

    /// Report whether this window is currently the key (active) window.
    ///
    /// # Returns
    ///
    /// `true` if the window is the key window, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Given an `IosWindow` instance `win`:
    /// let active = win.is_active();
    /// println!("Window active: {}", active);
    /// ```
    fn is_active(&self) -> bool {
        unsafe {
            let is_key: bool = msg_send![self.ui_window, isKeyWindow];
            is_key
        }
    }

    /// Report whether the window is currently hovered.
    ///
    /// On iOS this always returns `false` because pointer hover is not supported.
    ///
    /// # Examples
    ///
    /// ```
    /// // `window` is an `IosWindow`
    /// let hovered = window.is_hovered();
    /// assert_eq!(hovered, false);
    /// ```
    fn is_hovered(&self) -> bool {
        false // iOS doesn't have hover in the traditional sense
    }

    /// No-op setter for window title on iOS.
    ///
    /// iOS does not expose or use window titles in the UIKit windowing model, so this
    /// method intentionally does nothing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // On iOS this call has no effect.
    /// let mut window: IosWindow = /* obtain an IosWindow from your application */ unimplemented!();
    /// window.set_title("My App");
    /// ```
    fn set_title(&mut self, _title: &str) {
        // iOS windows don't have titles in the same way
    }

    /// Sets the window background appearance for platforms that support it.
    ///
    /// On iOS this method does not change visible behavior; the appearance parameter is accepted
    /// for API compatibility but is ignored.
    ///
    /// # Arguments
    ///
    /// * `appearance` - Desired background appearance to apply to the window (ignored on iOS).
    ///
    /// # Examples
    ///
    /// ```
    /// // `window` is an `IosWindow`
    /// window.set_background_appearance(WindowBackgroundAppearance::Dark);
    /// ```
    fn set_background_appearance(&self, _appearance: WindowBackgroundAppearance) {
        // Could adjust the view's background color
    }

    /// No-op on iOS; applications cannot programmatically minimize their windows.
    ///
    /// This method intentionally performs no action on iOS.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `win` is an `IosWindow` instance:
    /// // win.minimize(); // has no effect on iOS
    /// ```
    fn minimize(&self) {
        // iOS apps can't minimize themselves
    }

    /// No-op on iOS; window zooming is not applicable.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Obtain an `IosWindow` from your application setup, then:
    /// window.zoom();
    /// ```
    fn zoom(&self) {
        // iOS apps are always "zoomed"
    }

    /// No-op on iOS; iOS applications are always fullscreen.
    ///
    /// This method intentionally does nothing because an iOS window cannot be toggled
    /// between fullscreen and windowed states.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Calling this on iOS has no effect.
    /// let window = /* obtain an IosWindow instance */ unimplemented!();
    /// window.toggle_fullscreen();
    /// ```
    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    /// Report whether the window is currently fullscreen.
    ///
    /// # Returns
    ///
    /// `true` if the window is fullscreen, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // `window` is an `IosWindow`
    /// assert!(window.is_fullscreen());
    /// ```
    fn is_fullscreen(&self) -> bool {
        true
    }

    /// Registers a callback to be invoked when the window requests a render frame.
    ///
    /// The provided closure will be stored and later called with a `RequestFrameOptions` value
    /// each time the window requests a new frame.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given an existing `IosWindow` named `window`:
    /// window.on_request_frame(Box::new(|opts| {
    ///     // handle frame request, e.g. schedule render with `opts`
    ///     let _ = opts;
    /// }));
    /// ```
    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.state.request_frame_callback.lock() = Some(callback);
    }

    /// Registers a callback to receive translated platform input events for this window.
    ///
    /// The provided callback will be invoked with each `PlatformInput` and must return a
    /// `DispatchEventResult`. This replaces any previously registered input callback.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let window: IosWindow = /* obtain IosWindow instance */ unimplemented!();
    /// window.on_input(Box::new(|input: PlatformInput| {
    ///     // Inspect `input` and decide how to handle it.
    ///     DispatchEventResult::Ignored
    /// }));
    /// ```
    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.state.input_callback.lock() = Some(callback);
    }

    /// Registers a callback invoked when the window's active (key) status changes.
    ///
    /// The callback is called with `true` when the window becomes active (key window)
    /// and `false` when it resigns active status.
    ///
    /// # Examples
    ///
    /// ```
    /// // `window` is an `IosWindow`
    /// window.on_active_status_change(Box::new(|active| {
    ///     if active {
    ///         println!("window activated");
    ///     } else {
    ///         println!("window deactivated");
    ///     }
    /// }));
    /// ```
    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.state.active_status_callback.lock() = Some(callback);
    }

    /// Registers a callback that is called when the window's hover status changes.
    ///
    /// The provided callback is invoked with `true` when the window becomes hovered and `false` when it stops being hovered.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # // `win` is an existing `IosWindow`
    /// # fn _example(win: &crate::platform::ios::window::IosWindow) {
    /// let mut hovered = false;
    /// win.on_hover_status_change(Box::new(move |is_hovered| {
    ///     hovered = is_hovered;
    /// }));
    /// # }
    /// ```
    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.state.hover_status_callback.lock() = Some(callback);
    }

    /// Registers a callback to be invoked when the window's content size or scale changes.
    ///
    /// The provided callback is called with the new content size (in pixels) and the current scale factor
    /// whenever the view's drawable size, safe-area insets, or trait collection changes.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let window: IosWindow = /* obtain window */ unimplemented!();
    /// window.on_resize(Box::new(|size, scale| {
    ///     println!("Resized to {:?} at scale {}", size, scale);
    /// }));
    /// ```
    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.state.resize_callback.lock() = Some(callback);
    }

    /// Registers a callback to be invoked when the window moves.
    ///
    /// The provided callback will be stored and replace any previously registered moved callback.
    ///
    /// # Examples
    ///
    /// ```
    /// // `window` is an `IosWindow`.
    /// window.on_moved(Box::new(|| {
    ///     // handle move
    /// }));
    /// ```
    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.state.moved_callback.lock() = Some(callback);
    }

    /// Register a callback that decides whether the window should close.
    ///
    /// The provided closure is invoked when a close is requested; it must return `true` to allow the close
    /// or `false` to cancel it. This replaces any previously registered should-close callback.
    ///
    /// # Examples
    ///
    /// ```
    /// // Allow closing only if a condition is met
    /// let allow_close = std::cell::Cell::new(false);
    /// window.on_should_close(Box::new(move || allow_close.get()));
    /// ```
    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.state.should_close_callback.lock() = Some(callback);
    }

    /// Registers a one-time callback to run when the window is closed.
    ///
    /// The provided callback will be stored and invoked exactly once when the window is dropped/closed.
    ///
    /// # Examples
    ///
    /// ```
    /// // Register a callback to run on close
    /// let window: IosWindow = /* obtain window */ unimplemented!();
    /// window.on_close(Box::new(|| {
    ///     // cleanup actions
    /// }));
    /// ```
    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.state.close_callback.lock() = Some(callback);
    }

    /// Register a callback to be invoked when the window's appearance (for example, light/dark mode) changes.
    ///
    /// The callback is called when the underlying UIKit trait collection's `userInterfaceStyle` or related appearance traits change.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Given an `IosWindow` instance `window`:
    /// window.on_appearance_changed(Box::new(|| {
    ///     // React to appearance change (update UI, reload styles, etc.)
    /// }));
    /// ```
    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.state.appearance_changed_callback.lock() = Some(callback);
    }

    /// Registers a callback to determine whether a hit on the window should be treated as a window control area.
    ///
    /// The provided callback is invoked when the system needs to hit-test window chrome; it should return
    /// `Some(WindowControlArea)` to indicate which control area was hit or `None` to indicate no control area.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// window.on_hit_test_window_control(Box::new(|| Some(WindowControlArea::Close)));
    /// ```
    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.state.hit_test_callback.lock() = Some(callback);
    }

    /// Request rendering of the provided scene using the window's GPU renderer.
    ///
    /// Delegates the given `scene` to the window's internal renderer for drawing.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Create or obtain an `IosWindow` and a `Scene` elsewhere in your code:
    /// // let window: IosWindow = ...;
    /// // let scene: Scene = ...;
    /// window.draw(&scene);
    /// ```
    fn draw(&self, scene: &Scene) {
        self.state.renderer.lock().draw(scene);
    }

    /// Gets a shared handle to the renderer's sprite atlas.
    ///
    /// This returns an `Arc` pointing to the platform sprite atlas managed by the window's
    /// internal renderer so callers can share and use sprite resources.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let atlas = window.sprite_atlas();
    /// // Use `atlas` to access or clone sprite resources as needed.
    /// ```
    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.state.renderer.lock().sprite_atlas().clone()
    }

    /// Provides GPU capability information for the window's renderer when available.
    ///
    /// # Returns
    ///
    /// `Some(GpuSpecs)` with GPU information if available, `None` when GPU specifications are not exposed on this platform.
    ///
    /// # Examples
    ///
    /// ```
    /// // On iOS this currently returns None
    /// let specs = window.gpu_specs();
    /// assert!(specs.is_none());
    /// ```
    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    /// Ignores a requested window size change for iOS.
    ///
    /// The provided `_size` is intentionally ignored because iOS windows cannot be
    /// arbitrarily resized by applications.
    ///
    /// # Examples
    ///
    /// ```
    /// // Calling `resize` on an iOS window has no effect.
    /// // let mut window: IosWindow = /* obtain window */;
    /// // window.resize(Size::new(800, 600));
    /// ```
    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows can't be arbitrarily resized
    }

    /// Updates the input method editor (IME) anchor position for on-screen text input.
    ///
    /// On iOS this method is currently a no-op; callers may invoke it to provide the
    /// desired IME bounding box in window-local pixels but it will not affect system
    /// IME placement.
    ///
    /// # Parameters
    ///
    /// - `bounds`: The rectangle, in window-local pixels, where the IME (caret/candidate UI)
    ///   should be anchored.
    ///
    /// # Examples
    ///
    /// ```
    /// // Request IME to appear near a text field's caret.
    /// // `window` is an `IosWindow`; `bounds` is a `Bounds<Pixels>` describing the caret area.
    /// // On iOS this call is accepted but has no observable effect.
    /// // window.update_ime_position(bounds);
    /// ```
    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // Could position the software keyboard cursor indicator
    }
}

impl Drop for IosWindow {
    /// Cleans up native UI state and releases Rust-side resources when the window is dropped.
    ///
    /// This performs the observable teardown actions for the iOS window:
    /// - clears the stored native ivars that pointed to the shared window state,
    /// - invokes the registered close callback if present,
    /// - releases the Arc strong references that were held for the native ivars,
    /// - hides the native UIWindow.
    ///
    /// # Examples
    ///
    /// ```
    /// // `IosWindow` will have its resources released when it goes out of scope.
    /// // let window = IosWindow::new(...).unwrap();
    /// // drop(window);
    /// ```
    fn drop(&mut self) {
        unsafe {
            // Clear the state pointers
            (*self.view).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());
            (*self.view_controller).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());

            // Call any close callback
            if let Some(callback) = self.state.close_callback.lock().take() {
                callback();
            }

            // Decrement the Arc strong count for each ivar that held the pointer
            // (we incremented twice in new() - once for view, once for view_controller)
            Arc::decrement_strong_count(Arc::as_ptr(&self.state));
            Arc::decrement_strong_count(Arc::as_ptr(&self.state));

            // Hide the window
            let _: () = msg_send![self.ui_window, setHidden: YES];
        }
    }
}