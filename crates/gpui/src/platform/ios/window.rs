//! iOS window implementation using UIKit.
//!
//! This module provides a UIWindow with a custom UIView that hosts a CAMetalLayer
//! for GPU rendering. It handles touch input, keyboard events, and integrates
//! with the Metal renderer shared with macOS.

#![allow(unsafe_op_in_unsafe_fn)]

use super::{
    events::{translate_key_press, translate_pan_to_scroll, translate_touch_to_mouse},
    metal_renderer::MetalRenderer, BoolExt, CGPoint, CGRect, CGSize, UIEdgeInsets,
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
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
};

use super::IosDisplay;
use super::metal_renderer::InstanceBufferPool;

const WINDOW_STATE_IVAR: &str = "windowState";

static mut VIEW_CLASS: *const Class = ptr::null();
static mut VIEW_CONTROLLER_CLASS: *const Class = ptr::null();

/// Ensure the Objective-C classes are registered.
fn ensure_classes_registered() {
    unsafe {
        if VIEW_CLASS.is_null() {
            VIEW_CLASS = register_view_class();
        }
        if VIEW_CONTROLLER_CLASS.is_null() {
            VIEW_CONTROLLER_CLASS = register_view_controller_class();
        }
    }
}

/// Register the custom UIView subclass for GPUI rendering.
unsafe fn register_view_class() -> *const Class {
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
unsafe fn register_view_controller_class() -> *const Class {
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
    class!(CAMetalLayer)
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

extern "C" fn presses_began(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    handle_presses(this, presses, true);
}

extern "C" fn presses_ended(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    handle_presses(this, presses, false);
}

extern "C" fn presses_changed(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    // Changed is used for pressure-sensitive keys, treat as key down
    handle_presses(this, presses, true);
}

extern "C" fn presses_cancelled(this: &Object, _sel: Sel, presses: *mut Object, _event: *mut Object) {
    handle_presses(this, presses, false);
}

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
            
            if let Some(event) = translate_key_press(press, is_key_down, is_repeat) {
                dispatch_event(&state, event);
            }
        }
    }
}

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

extern "C" fn safe_area_insets_did_change(this: &Object, _sel: Sel) {
    unsafe {
        // Call super
        let superclass = class!(UIViewController);
        let _: () = msg_send![super(this, superclass), viewSafeAreaInsetsDidChange];
        
        // Could notify about safe area changes here
    }
}

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

unsafe fn get_window_state(view: &Object) -> *const WindowState {
    let state_ptr: *mut c_void = *view.get_ivar(WINDOW_STATE_IVAR);
    state_ptr as *const WindowState
}

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
            let view_controller: *mut Object = msg_send![VIEW_CONTROLLER_CLASS, alloc];
            let view_controller: *mut Object = msg_send![view_controller, init];
            
            // Create the custom view
            let view: *mut Object = msg_send![VIEW_CLASS, alloc];
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
            
            // Keep the Arc alive
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
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        unsafe {
            let (_tx, rx) = oneshot::channel();
            
            // Create UIAlertController
            let title = super::ns_string(msg);
            let message = detail.map(|d| super::ns_string(d)).unwrap_or(ptr::null_mut());
            
            let style: i64 = 1; // UIAlertControllerStyleAlert
            let alert: *mut Object = msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: title
                message: message
                preferredStyle: style
            ];
            
            // Add buttons
            for (_index, button) in answers.iter().enumerate() {
                let button_title = super::ns_string(button.label());
                let action_style: i64 = if button.is_cancel() { 1 } else { 0 }; // UIAlertActionStyleCancel/Default
                
                // Note: In a full implementation, we'd need to properly capture
                // the index and sender in the action handler
                let action: *mut Object = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: button_title
                    style: action_style
                    handler: ptr::null::<c_void>()
                ];
                
                let _: () = msg_send![alert, addAction: action];
            }
            
            // Present the alert
            let _: () = msg_send![
                self.view_controller,
                presentViewController: alert
                animated: YES
                completion: ptr::null::<c_void>()
            ];
            
            Some(rx)
        }
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

    fn update_ime_position(&self, _bounds: Bounds<ScaledPixels>) {
        // Could position the software keyboard cursor indicator
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        unsafe {
            // Clear the state pointers
            (*self.view).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());
            (*self.view_controller).set_ivar(WINDOW_STATE_IVAR, ptr::null_mut::<c_void>());
            
            // Call any close callback
            if let Some(callback) = self.state.close_callback.lock().take() {
                callback();
            }
            
            // Decrement the Arc strong count that we incremented in new()
            Arc::decrement_strong_count(Arc::as_ptr(&self.state));
            
            // Hide the window
            let _: () = msg_send![self.ui_window, setHidden: YES];
        }
    }
}
