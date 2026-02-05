//! iOS window implementation using UIKit.
//!
//! This module provides a UIWindow with a custom UIView that hosts a CAMetalLayer
//! for GPU rendering. It handles touch input, keyboard events, and integrates
//! with the Metal renderer shared with macOS.

use super::{
    BoolExt, CGPoint, CGRect, CGSize, UIEdgeInsets,
    events::{
        UIKeyModifierFlags, ios_keyboard_log, translate_key_press, translate_modifiers_changed,
        translate_pan_to_scroll, translate_touch_to_mouse,
    },
    metal_renderer::MetalRenderer,
};
use super::metal_atlas::MetalAtlas;
use crate::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, GpuSpecs, Modifiers, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, ScaledPixels, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams, point, px, size,
    platform::PlatformInputHandler,
};
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
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    UiKitDisplayHandle, UiKitWindowHandle,
};
use std::{
    cell::RefCell,
    ffi::c_void,
    ptr,
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, OnceLock, Weak},
};

use super::IosDisplay;
use super::metal_renderer::InstanceBufferPool;

const WINDOW_STATE_IVAR: &str = "windowState";

unsafe extern "C" {
    static NSRunLoopCommonModes: *mut Object;
}

static VIEW_CLASS: OnceLock<&'static Class> = OnceLock::new();
static VIEW_CONTROLLER_CLASS: OnceLock<&'static Class> = OnceLock::new();

/// Ensure the Objective-C classes are registered.
fn ensure_classes_registered() {
    VIEW_CLASS.get_or_init(|| unsafe { register_view_class() });
    VIEW_CONTROLLER_CLASS.get_or_init(|| unsafe { register_view_controller_class() });
}

/// Register the custom UIView subclass for GPUI rendering.
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

    // UIKeyInput protocol for software keyboard support
    if let Some(protocol) = Protocol::get("UIKeyInput") {
        decl.add_protocol(protocol);
    }
    decl.add_method(
        sel!(insertText:),
        insert_text as extern "C" fn(&Object, Sel, *mut Object),
    );
    decl.add_method(
        sel!(deleteBackward),
        delete_backward as extern "C" fn(&Object, Sel),
    );
    decl.add_method(
        sel!(hasText),
        has_text as extern "C" fn(&Object, Sel) -> BOOL,
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

extern "C" fn layer_class(_this: &Class, _sel: Sel) -> *const Class {
    // Return regular CALayer as the view's layer class. The Metal renderer's
    // CAMetalLayer is added as a sublayer instead of making the view's root
    // layer itself a CAMetalLayer.
    class!(CALayer)
}

extern "C" fn can_become_first_responder(_this: &Object, _sel: Sel) -> BOOL {
    YES
}

extern "C" fn touches_began(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "began");
}

extern "C" fn touches_moved(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "moved");
}

extern "C" fn touches_ended(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "ended");
}

extern "C" fn touches_cancelled(this: &Object, _sel: Sel, touches: *mut Object, _event: *mut Object) {
    handle_touches(this, touches, "cancelled");
}

fn handle_touches(view: &Object, touches: *mut Object, _phase: &str) {
    unsafe {
        let Some(state) = get_window_state(view) else {
            return;
        };

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

extern "C" fn presses_began(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Use minimal handler for debugging
        handle_presses(this, presses, true);
    }));
    if let Err(e) = result {
        log::error!("Panic in presses_began: {:?}", e);
    }
}

extern "C" fn presses_ended(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Use minimal handler for debugging
        handle_presses(this, presses, false);
    }));
    if let Err(e) = result {
        log::error!("Panic in presses_ended: {:?}", e);
    }
}

extern "C" fn presses_changed(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    // Changed is used for pressure-sensitive keys, treat as key down
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_presses(this, presses, true);
    }));
    if let Err(e) = result {
        log::error!("Panic in presses_changed: {:?}", e);
    }
}

extern "C" fn presses_cancelled(
    this: &Object,
    _sel: Sel,
    presses: *mut Object,
    _event: *mut Object,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_presses(this, presses, false);
    }));
    if let Err(e) = result {
        log::error!("Panic in presses_cancelled: {:?}", e);
    }
}

fn handle_presses(view: &Object, presses: *mut Object, is_key_down: bool) {
    unsafe {
        // Early null check for presses
        if presses.is_null() {
            ios_keyboard_log("handle_presses: presses is null, returning early");
            return;
        }

        let Some(state) = get_window_state(view) else {
            ios_keyboard_log("handle_presses: could not get window state");
            return;
        };

        let count: usize = msg_send![presses, count];
        ios_keyboard_log(&format!(
            "handle_presses: count={}, is_key_down={}",
            count, is_key_down
        ));

        if count == 0 {
            return;
        }

        let all_objects: *mut Object = msg_send![presses, allObjects];
        if all_objects.is_null() {
            ios_keyboard_log("handle_presses: allObjects is null");
            return;
        }

        for i in 0..count {
            let press: *mut Object = msg_send![all_objects, objectAtIndex: i];
            if press.is_null() {
                ios_keyboard_log(&format!("handle_presses: press {} is null, skipping", i));
                continue;
            }
            ios_keyboard_log(&format!("Processing press {}/{}", i + 1, count));

            // Check if this is a repeat - key may be null for non-keyboard presses
            let key: *mut Object = msg_send![press, key];
            let is_repeat: bool = if !key.is_null() {
                msg_send![press, isRepeating]
            } else {
                ios_keyboard_log("handle_presses: key is null (not a keyboard press?)");
                false
            };

            // Skip non-keyboard presses entirely
            if key.is_null() {
                ios_keyboard_log("handle_presses: skipping non-keyboard press");
                continue;
            }

            // Update modifier state from hardware keyboard and dispatch ModifiersChangedEvent if changed
            let modifier_flags: i64 = msg_send![key, modifierFlags];
            let new_modifiers = UIKeyModifierFlags(modifier_flags).to_modifiers();
            let old_modifiers = state.modifiers.lock().clone();
            if new_modifiers != old_modifiers {
                *state.modifiers.lock() = new_modifiers;
                let modifiers_event = translate_modifiers_changed(modifier_flags);
                dispatch_event(&state, modifiers_event);
            }

            // Translate the key press - this should not panic now with all the null checks
            if let Some(event) = translate_key_press(press, is_key_down, is_repeat) {
                ios_keyboard_log(&format!("Dispatching key event for press {}", i + 1));
                dispatch_event(&state, event);
            } else {
                ios_keyboard_log(&format!("No event generated for press {}", i + 1));
            }
        }
    }
}

extern "C" fn insert_text(this: &Object, _sel: Sel, text: *mut Object) {
    unsafe {
        let Some(state) = get_window_state(this) else {
            return;
        };

        if text.is_null() {
            return;
        }

        let c_str: *const i8 = msg_send![text, UTF8String];
        if c_str.is_null() {
            return;
        }

        let text_str = match std::ffi::CStr::from_ptr(c_str).to_str() {
            Ok(s) => s,
            Err(_) => return,
        };

        if text_str.is_empty() {
            return;
        }

        // First try to use the input handler if one is registered
        let handled = with_input_handler(&state, |input_handler| {
            input_handler.replace_text_in_range(None, text_str);
        })
        .is_some();

        // If no input handler, dispatch as a KeyDown event with key_char
        // so that elements using on_key_down can receive it
        if !handled {
            let keystroke = crate::Keystroke {
                key: text_str.into(),
                modifiers: Modifiers::default(),
                key_char: Some(text_str.to_string()),
            };

            let event = PlatformInput::KeyDown(crate::KeyDownEvent {
                keystroke,
                is_held: false,
                prefer_character_input: true,
            });

            dispatch_event(&state, event);
        }
    }
}

extern "C" fn delete_backward(this: &Object, _sel: Sel) {
    unsafe {
        let Some(state) = get_window_state(this) else {
            return;
        };

        let keystroke = crate::Keystroke {
            key: "backspace".into(),
            modifiers: Modifiers::default(),
            key_char: None,
        };

        let event = PlatformInput::KeyDown(crate::KeyDownEvent {
            keystroke,
            is_held: false,
            prefer_character_input: false,
        });

        dispatch_event(&state, event);
    }
}

extern "C" fn has_text(this: &Object, _sel: Sel) -> BOOL {
    unsafe {
        let Some(state) = get_window_state(this) else {
            return NO;
        };

        let has = with_input_handler(&state, |input_handler| {
            // Check if there's a selection with text, or if cursor is past position 0
            if let Some(sel) = input_handler.selected_text_range(false) {
                if sel.range.len() > 0 || sel.range.start > 0 {
                    return true;
                }
            }

            // Cursor might be at position 0, but document could still have text.
            // Check if there's any text in the document by requesting text from the start.
            let mut adjusted = None;
            input_handler
                .text_for_range(0..1, &mut adjusted)
                .is_some_and(|text| !text.is_empty())
        })
        .unwrap_or(false);

        if has { YES } else { NO }
    }
}

fn with_input_handler<F, R>(state: &WindowState, f: F) -> Option<R>
where
    F: FnOnce(&mut crate::platform::PlatformInputHandler) -> R,
{
    let mut input_handler = state.input_handler.lock().take()?;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut input_handler)));

    // Always restore the input handler, even if f panicked
    *state.input_handler.lock() = Some(input_handler);

    match result {
        Ok(value) => Some(value),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

extern "C" fn layout_subviews(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIView);
        let _: () = msg_send![super(this, superclass), layoutSubviews];

        let Some(state) = get_window_state(this) else {
            return;
        };

        // Update Metal layer size - we need to update the renderer's layer, not the view's layer
        let bounds: CGRect = msg_send![this, bounds];
        let scale: f64 = msg_send![this, contentScaleFactor];

        // Update renderer's layer frame and drawable size
        {
            let renderer = state.renderer.lock();
            let renderer_layer = renderer.layer_ptr();
            let _: () = msg_send![renderer_layer, setFrame: bounds];
            let _: () = msg_send![renderer_layer, setContentsScale: scale];
            let drawable_size = CGSize {
                width: bounds.size.width * scale,
                height: bounds.size.height * scale,
            };
            let _: () = msg_send![renderer_layer, setDrawableSize: drawable_size];
        }

        // Update scale factor in state
        *state.scale_factor.lock() = scale as f32;

        // Notify of resize
        if let Some(callback) = state.resize_callback.lock().as_mut() {
            callback(
                size(px(bounds.size.width as f32), px(bounds.size.height as f32)),
                scale as f32,
            );
        }
    }
}

extern "C" fn display_layer(this: &Object, _sel: Sel, _layer: *mut Object) {
    unsafe {
        let Some(state) = get_window_state(this) else {
            return;
        };

        if let Some(callback) = state.request_frame_callback.lock().as_mut() {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render: false,
            });
        }
    }
}

extern "C" fn view_did_load(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewDidLoad];
    }
}

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
        let Some(state) = get_window_state(this) else {
            return;
        };

        if let Some(callback) = state.resize_callback.lock().as_mut() {
            let scale = *state.scale_factor.lock();
            callback(
                size(px(new_size.width as f32), px(new_size.height as f32)),
                scale,
            );
        }
    }
}

extern "C" fn safe_area_insets_did_change(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewSafeAreaInsetsDidChange];

        // Notify about safe area changes by treating them as a layout/resize event.
        let Some(state) = get_window_state(this) else {
            return;
        };

        if let Some(callback) = state.resize_callback.lock().as_mut() {
            // Obtain the current view size in points and convert to pixels.
            let view: *mut Object = msg_send![this, view];
            if !view.is_null() {
                let bounds: CGRect = msg_send![view, bounds];
                let bounds_size = bounds.size;
                let scale = *state.scale_factor.lock();
                callback(
                    size(px(bounds_size.width as f32), px(bounds_size.height as f32)),
                    scale,
                );
            }
        }
    }
}

extern "C" fn trait_collection_did_change(this: &Object, _sel: Sel, previous: *mut Object) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), traitCollectionDidChange: previous];

        let Some(state) = get_window_state(this) else {
            return;
        };

        if let Some(callback) = state.appearance_changed_callback.lock().as_mut() {
            callback();
        }
    }
}

unsafe fn get_window_state(obj: &Object) -> Option<Arc<WindowState>> {
    let weak_ptr: *mut c_void = *obj.get_ivar(WINDOW_STATE_IVAR);
    if weak_ptr.is_null() {
        return None;
    }

    let weak = &*(weak_ptr as *const Weak<WindowState>);
    weak.upgrade()
}

fn dispatch_event(state: &WindowState, event: PlatformInput) {
    if let Some(callback) = state.input_callback.lock().as_mut() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(event);
        }));
        if let Err(e) = result {
            log::error!("Panic in dispatch_event: {:?}", e);
        }
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
    display_link: Mutex<Option<*mut Object>>,
}

/// iOS window implementation.
pub struct IosWindow {
    ui_window: *mut Object,
    view: *mut Object,
    view_controller: *mut Object,
    state: Arc<WindowState>,
    view_weak: Box<Weak<WindowState>>,
    view_controller_weak: Box<Weak<WindowState>>,
    display_link_target: *mut Object,
    display_link_target_weak: Box<Weak<WindowState>>,
}

static DISPLAY_LINK_TARGET_CLASS: OnceLock<&'static Class> = OnceLock::new();

fn ensure_display_link_class_registered() {
    DISPLAY_LINK_TARGET_CLASS.get_or_init(|| unsafe { register_display_link_target_class() });
}

unsafe fn register_display_link_target_class() -> &'static Class {
    let superclass = class!(NSObject);
    let mut decl = ClassDecl::new("GPUIDisplayLinkTarget", superclass).unwrap();

    decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);

    decl.add_method(
        sel!(step:),
        display_link_step as extern "C" fn(&Object, Sel, *mut Object),
    );

    decl.register()
}

extern "C" fn display_link_step(this: &Object, _sel: Sel, _display_link: *mut Object) {
    unsafe {
        let Some(state) = get_window_state(this) else {
            return;
        };

        if let Some(callback) = state.request_frame_callback.lock().as_mut() {
            callback(RequestFrameOptions {
                require_presentation: true,
                force_render: false,
            });
        }
    }
}

unsafe impl Send for IosWindow {}

impl IosWindow {
    pub fn new(
        handle: AnyWindowHandle,
        _params: WindowParams,
        renderer_context: Arc<Mutex<InstanceBufferPool>>,
    ) -> anyhow::Result<Self> {
        ensure_classes_registered();
        ensure_display_link_class_registered();

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

            let renderer = MetalRenderer::new(renderer_context);
            let view_layer: *mut Object = msg_send![view, layer];
            let renderer_layer = renderer.layer_ptr();

            let clear_color: *mut Object = msg_send![class!(UIColor), clearColor];
            let _: () = msg_send![view, setBackgroundColor: clear_color];
            let _: () = msg_send![view_layer, setBackgroundColor: std::ptr::null::<c_void>()];

            let _: () = msg_send![view_layer, addSublayer: renderer_layer];

            let _: () = msg_send![renderer_layer, setFrame: screen_bounds];
            let _: () = msg_send![renderer_layer, setContentsScale: scale];
            let drawable_size = CGSize {
                width: screen_bounds.size.width * scale,
                height: screen_bounds.size.height * scale,
            };
            let _: () = msg_send![renderer_layer, setDrawableSize: drawable_size];

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
                display_link: Mutex::new(None),
            });

            let view_weak = Box::new(Arc::downgrade(&state));
            let view_controller_weak = Box::new(Arc::downgrade(&state));
            let display_link_target_weak = Box::new(Arc::downgrade(&state));

            let view_weak_ptr = view_weak.as_ref() as *const Weak<WindowState> as *mut c_void;
            (*view).set_ivar(WINDOW_STATE_IVAR, view_weak_ptr);
            let vc_weak_ptr =
                view_controller_weak.as_ref() as *const Weak<WindowState> as *mut c_void;
            (*view_controller).set_ivar(WINDOW_STATE_IVAR, vc_weak_ptr);

            let display_link_target_class = *DISPLAY_LINK_TARGET_CLASS.get().unwrap();
            let display_link_target: *mut Object = msg_send![display_link_target_class, alloc];
            let display_link_target: *mut Object = msg_send![display_link_target, init];
            let dl_weak_ptr =
                display_link_target_weak.as_ref() as *const Weak<WindowState> as *mut c_void;
            (*display_link_target).set_ivar(WINDOW_STATE_IVAR, dl_weak_ptr);

            let display_link: *mut Object = msg_send![
                class!(CADisplayLink),
                displayLinkWithTarget: display_link_target
                selector: sel!(step:)
            ];

            let run_loop: *mut Object = msg_send![class!(NSRunLoop), mainRunLoop];
            let _: () =
                msg_send![display_link, addToRunLoop: run_loop forMode: NSRunLoopCommonModes];

            *state.display_link.lock() = Some(display_link);
            // Set up the view hierarchy
            let _: () = msg_send![view_controller, setView: view];
            let _: () = msg_send![ui_window, setRootViewController: view_controller];

            // Make the view first responder to receive keyboard events
            let _: () = msg_send![view, becomeFirstResponder];

            // Make the window visible
            let _: () = msg_send![ui_window, makeKeyAndVisible];

            // Request initial display - this ensures the layer gets drawn
            let _: () = msg_send![view, setNeedsDisplay];
            let _: () = msg_send![renderer_layer, setNeedsDisplay];

            Ok(Self {
                ui_window,
                view,
                view_controller,
                state,
                view_weak,
                view_controller_weak,
                display_link_target,
                display_link_target_weak,
            })
        }
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(&self) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        // UiKitWindowHandle::new takes the ui_view (not ui_window)
        let mut handle = UiKitWindowHandle::new(
            NonNull::new(self.view as *mut c_void).unwrap(),
        );
        handle.ui_view_controller = NonNull::new(self.view_controller as *mut c_void);

        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(RawWindowHandle::UiKit(handle)) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(&self) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe {
            raw_window_handle::DisplayHandle::borrow_raw(RawDisplayHandle::UiKit(UiKitDisplayHandle::new()))
        })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        unsafe {
            let frame: CGRect = msg_send![self.view, frame];
            Bounds {
                origin: point(px(frame.origin.x as f32), px(frame.origin.y as f32)),
                size: size(px(frame.size.width as f32), px(frame.size.height as f32)),
            }
        }
    }

    fn is_maximized(&self) -> bool {
        true // iOS windows are always "maximized"
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        unsafe {
            let bounds: CGRect = msg_send![self.view, bounds];
            let insets: UIEdgeInsets = msg_send![self.view, safeAreaInsets];

            size(
                px((bounds.size.width - insets.left - insets.right) as f32),
                px((bounds.size.height - insets.top - insets.bottom) as f32),
            )
        }
    }

    fn scale_factor(&self) -> f32 {
        *self.state.scale_factor.lock()
    }

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

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        // iOS doesn't have a persistent mouse position
        // Return center of the view as a reasonable default
        let bounds = self.bounds();
        point(
            bounds.size.width / 2.0,
            bounds.size.height / 2.0,
        )
    }

    fn modifiers(&self) -> Modifiers {
        self.state.modifiers.lock().clone()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.state.input_handler.lock() = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.state.input_handler.lock().take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // TODO: Implement UIAlertController with proper action handlers
        // that capture tx and send the selected button index
        None
    }

    fn activate(&self) {
        unsafe {
            let _: () = msg_send![self.ui_window, makeKeyAndVisible];
        }
    }

    fn is_active(&self) -> bool {
        unsafe {
            let is_key: bool = msg_send![self.ui_window, isKeyWindow];
            is_key
        }
    }

    fn is_hovered(&self) -> bool {
        false // iOS doesn't have hover in the traditional sense
    }

    fn set_title(&mut self, _title: &str) {
        // iOS windows don't have titles in the same way
    }

    fn set_background_appearance(&self, _appearance: WindowBackgroundAppearance) {
        // Could adjust the view's background color
    }

    fn minimize(&self) {
        // iOS apps can't minimize themselves
    }

    fn zoom(&self) {
        // iOS apps are always "zoomed"
    }

    fn toggle_fullscreen(&self) {
        // iOS apps are always fullscreen
    }

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.state.request_frame_callback.lock() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.state.input_callback.lock() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.state.active_status_callback.lock() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.state.hover_status_callback.lock() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.state.resize_callback.lock() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.state.moved_callback.lock() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.state.should_close_callback.lock() = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.state.close_callback.lock() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.state.appearance_changed_callback.lock() = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.state.hit_test_callback.lock() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.state.renderer.lock().draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.state.renderer.lock().sprite_atlas().clone()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // iOS windows can't be arbitrarily resized
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // Could position the software keyboard cursor indicator
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        unsafe {
            if let Some(display_link) = self.state.display_link.lock().take() {
                let _: () = msg_send![display_link, invalidate];
            }

            (*self.view).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());
            (*self.view_controller).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());
            (*self.display_link_target).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());

            let _: () = msg_send![self.display_link_target, release];

            // Call any close callback
            if let Some(callback) = self.state.close_callback.lock().take() {
                callback();
            }

            // Hide the window
            let _: () = msg_send![self.ui_window, setHidden: YES];
        }
    }
}
