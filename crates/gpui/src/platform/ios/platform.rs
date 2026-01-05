//! iOS platform implementation.
//!
//! This module provides the main platform interface for iOS, implementing the
//! `Platform` trait with iOS-specific functionality.

use super::{IosDispatcher, IosDisplay, IosWindow, ns_string, renderer};
use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardEntry, ClipboardItem, ForegroundExecutor,
    Keymap, Menu, MenuItem, PathPromptOptions, Platform, PlatformDisplay,
    PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Result, Task,
    WindowAppearance, WindowParams, DummyKeyboardMapper,
};
use anyhow::anyhow;
use futures::channel::oneshot;
use objc::{class, msg_send, runtime::Object, sel, sel_impl};
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    ffi::c_void,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

pub(crate) struct IosPlatform(Mutex<IosPlatformState>);

struct IosPlatformState {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    renderer_context: renderer::Context,

    // Callbacks
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit_callback: Option<Box<dyn FnMut()>>,

    // Active windows tracking
    windows: Vec<AnyWindowHandle>,
}

impl IosPlatform {
    /// Creates a new IosPlatform instance initialized for use on iOS.
    ///
    /// The instance includes configured foreground and background executors, a platform
    /// text system (feature-dependent), a default renderer context, and empty callbacks
    /// and window list.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = gpui::platform::ios::IosPlatform::new();
    /// ```
    pub fn new() -> Self {
        let dispatcher = Arc::new(IosDispatcher::new());

        #[cfg(feature = "font-kit")]
        let text_system = Arc::new(crate::platform::ios::text_system::MacTextSystem::new());

        #[cfg(not(feature = "font-kit"))]
        let text_system = Arc::new(crate::NoopTextSystem::new());

        Self(Mutex::new(IosPlatformState {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            renderer_context: renderer::Context::default(),
            open_urls: None,
            quit_callback: None,
            windows: Vec::new(),
        }))
    }

    /// Called from the iOS app delegate when the app finishes launching.
    pub fn did_finish_launching(&self, on_finish_launching: Box<dyn FnOnce()>) {
        on_finish_launching();
    }

    /// Invokes the stored "open URLs" handler with the provided list of URL strings.
    ///
    /// If an on-open-URLs callback has been registered, it will be called with `urls`.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `platform` is an initialized `IosPlatform`.
    /// let platform = IosPlatform::new();
    /// platform.handle_open_urls(vec!["https://example.com".into()]);
    /// ```
    pub fn handle_open_urls(&self, urls: Vec<String>) {
        let mut state = self.0.lock();
        if let Some(callback) = state.open_urls.as_mut() {
            callback(urls);
        }
    }

    /// Invoke the registered quit callback, if any, when the app is about to terminate.
    ///
    /// If a quit callback has been set via `on_quit`, it will be called; otherwise this does nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use std::cell::Cell;
    ///
    /// // `platform` is an instance of `IosPlatform` with an `on_quit` method.
    /// let platform = /* create or obtain IosPlatform instance */;
    ///
    /// let called = Rc::new(Cell::new(false));
    /// let c = called.clone();
    /// platform.on_quit(move || c.set(true));
    ///
    /// platform.will_terminate();
    /// assert!(called.get());
    /// ```
    pub fn will_terminate(&self) {
        let mut state = self.0.lock();
        if let Some(callback) = state.quit_callback.as_mut() {
            callback();
        }
    }

    /// Gets a clone of the platform's renderer context.
    ///
    /// # Returns
    ///
    /// `renderer::Context` cloned from the platform state.
    ///
    /// # Examples
    ///
    /// ```
    /// // Assuming `platform` is an `IosPlatform` instance:
    /// let ctx = platform.renderer_context();
    /// // Use `ctx` with renderer APIs...
    /// ```
    fn renderer_context(&self) -> renderer::Context {
        self.0.lock().renderer_context.clone()
    }
}

impl Platform for IosPlatform {
    /// Accesses the platform's background task executor.
    ///
    /// Returns a clone of the platform's `BackgroundExecutor`.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let bg_exec = platform.background_executor();
    /// // use `bg_exec` to spawn background tasks
    /// ```
    fn background_executor(&self) -> BackgroundExecutor {
        self.0.lock().background_executor.clone()
    }

    /// Accesses the platform's foreground executor.
    ///
    /// # Returns
    ///
    /// A `ForegroundExecutor` cloned from the platform state.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let exec = platform.foreground_executor();
    /// // `exec` can be used to schedule tasks on the foreground executor.
    /// ```
    fn foreground_executor(&self) -> ForegroundExecutor {
        self.0.lock().foreground_executor.clone()
    }

    /// Retrieves the current platform text system.
    ///
    /// # Examples
    ///
    /// ```
    /// let text_system = platform.text_system();
    /// // `text_system` can now be used to query or create text layouts.
    /// ```
    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.0.lock().text_system.clone()
    }

    /// Calls the provided finish-launching callback immediately, because UIKit manages the main run loop on iOS.
    ///
    /// # Examples
    ///
    /// ```
    /// // Invoke the platform run hook with a finish-launching callback.
    /// platform.run(Box::new(|| {
    ///     // Perform post-launch initialization here.
    /// }));
    /// ```
    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        // On iOS, UIApplicationMain owns and manages the main run loop.
        // Unlike macOS where we need to explicitly run NSRunLoop, on iOS
        // the run loop is already running by the time this is called.
        // We only need to invoke the finish launching callback and return
        // immediately to avoid blocking the UIKit event handling.
        on_finish_launching();
    }

    /// Notify the platform that the application is quitting by invoking the registered quit callback.
    ///
    /// On iOS the system manages application lifecycle and applications must not terminate themselves;
    /// this function only triggers the stored quit callback (if one is set) so the embedder can perform cleanup.
    fn quit(&self) {
        // iOS apps don't quit programmatically in the traditional sense.
        // The system manages app lifecycle.
        // We can notify the callback though.
        self.will_terminate();
    }

    /// Performs no action on iOS; the platform does not support programmatic app restart.
    ///
    /// The provided `binary_path`, if any, is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// // On iOS this will be a no-op.
    /// let platform = /* IosPlatform instance */;
    /// platform.restart(None);
    /// ```
    fn restart(&self, _binary_path: Option<PathBuf>) {
        // Not supported on iOS - apps cannot restart themselves
    }

    /// No-op on iOS; the system manages application activation.
    ///
    /// Calling this method has no effect on iOS.
    ///
    /// # Examples
    ///
    /// ```
    /// // calling activate is permitted but does nothing on iOS
    /// platform.activate(true);
    /// ```
    fn activate(&self, _ignoring_other_apps: bool) {
        // iOS apps are activated by the system, not programmatically
    }

    /// No-op on iOS; iOS does not provide a programmatic "hide" concept.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // On iOS the system manages visibility; calling `hide` has no effect.
    /// let platform = /* IosPlatform instance */ unimplemented!();
    /// platform.hide();
    /// ```
    fn hide(&self) {
        // iOS doesn't have a "hide" concept like macOS
    }

    /// No-op placeholder for hiding other applications on platforms that support it.
    ///
    /// On iOS this function does nothing; the system does not expose an API to hide other apps.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = gpui::platform::ios::platform::IosPlatform::new();
    /// platform.hide_other_apps();
    /// ```
    fn hide_other_apps(&self) {
        // Not applicable on iOS
    }

    /// No-op on iOS; iOS does not support unhiding other applications.
    ///
    /// # Examples
    ///
    /// ```
    /// // Calling this on iOS has no effect.
    /// // let platform = IosPlatform::new(...);
    /// // platform.unhide_other_apps();
    /// ```
    fn unhide_other_apps(&self) {
        // Not applicable on iOS
    }

    /// List all available iOS displays as platform display handles.
    ///
    /// Returns a vector of `Rc<dyn PlatformDisplay>` where each element represents an
    /// available UIScreen converted to the platform display abstraction.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let displays = platform.displays();
    /// // use `displays` to inspect available screens
    /// ```
    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        IosDisplay::all()
            .into_iter()
            .map(|d| Rc::new(d) as Rc<dyn PlatformDisplay>)
            .collect()
    }

    /// Get the device's main iOS display.
    ///
    /// # Returns
    ///
    /// An `Rc<dyn PlatformDisplay>` wrapping the device's primary display.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let primary = platform.primary_display();
    /// assert!(primary.is_some());
    /// ```
    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    /// Get the first active window handle, if present.
    ///
    /// Returns `Some(handle)` with the first active `AnyWindowHandle`, or `None` if no windows are active.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Obtain an `IosPlatform` instance from your application setup, then:
    /// // let platform: IosPlatform = ...;
    /// // let active = platform.active_window();
    /// // assert!(active.is_none() || active.is_some());
    /// ```
    fn active_window(&self) -> Option<AnyWindowHandle> {
        let state = self.0.lock();
        state.windows.first().copied()
    }

    /// Creates and registers a new iOS platform window for the given native handle and window parameters.
    ///
    /// The provided `handle` is appended to the platform's active window list before constructing the
    /// platform-specific window wrapper.
    ///
    /// # Parameters
    ///
    /// - `handle`: The native window handle to open and track.
    /// - `options`: Window creation parameters (size, style, etc.).
    ///
    /// # Returns
    ///
    /// `Box<dyn PlatformWindow>` containing the newly created platform window on success, or an `anyhow::Error` on failure.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use gpui::platform::{IosPlatform, WindowParams, AnyWindowHandle, Platform};
    /// # let platform = IosPlatform::new();
    /// # let handle: AnyWindowHandle = /* obtain native handle from host */ unimplemented!();
    /// # let params = WindowParams::default();
    /// let window = platform.open_window(handle, params).expect("failed to open window");
    /// ```
    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let mut state = self.0.lock();
        state.windows.push(handle);

        let renderer_context = state.renderer_context.clone();
        drop(state);

        let window = IosWindow::new(handle, options, renderer_context)?;
        Ok(Box::new(window))
    }

    /// Detects the current UI appearance (light or dark) from the main screen's trait collection.
    ///
    /// # Returns
    ///
    /// `WindowAppearance::Dark` if `UIUserInterfaceStyleDark` is active, `WindowAppearance::Light` otherwise.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assuming `platform` is an `IosPlatform` instance:
    /// // let platform = IosPlatform::new();
    /// // let appearance = platform.window_appearance();
    /// // assert!(matches!(appearance, WindowAppearance::Dark | WindowAppearance::Light));
    /// ```
    fn window_appearance(&self) -> WindowAppearance {
        unsafe {
            // Check the current trait collection for dark/light mode
            let screen: *mut Object = msg_send![class!(UIScreen), mainScreen];
            let trait_collection: *mut Object = msg_send![screen, traitCollection];
            let style: i64 = msg_send![trait_collection, userInterfaceStyle];

            match style {
                2 => WindowAppearance::Dark,      // UIUserInterfaceStyleDark
                _ => WindowAppearance::Light,    // UIUserInterfaceStyleLight or Unspecified
            }
        }
    }

    /// Requests the system to open the given URL string using UIApplication.
    ///
    /// The provided `url` should be a valid URL string (for example, "https://example.com").
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // Assume `platform` is an instance of the iOS platform wrapper.
    /// // platform.open_url("https://example.com");
    /// ```
    fn open_url(&self, url: &str) {
        unsafe {
            let url_string = ns_string(url);
            let nsurl: *mut Object = msg_send![class!(NSURL), URLWithString: url_string];
            let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
            let _: () = msg_send![app, openURL: nsurl options: std::ptr::null::<Object>() completionHandler: std::ptr::null::<c_void>()];
        }
    }

    /// Register a callback to be invoked when the app is asked to open one or more URLs.
    ///
    /// The provided callback will be stored and called with a `Vec<String>` containing the URL(s)
    /// that the application should open.
    ///
    /// # Examples
    ///
    /// ```
    /// # use crate::platform::ios::platform::IosPlatform;
    /// let platform: IosPlatform = unimplemented!(); // obtain platform instance from your app initialization
    /// platform.on_open_urls(Box::new(|urls: Vec<String>| {
    ///     // handle opened URLs
    ///     assert!(!urls.is_empty());
    /// }));
    /// ```
    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.0.lock().open_urls = Some(callback);
    }

    /// Registers a URL scheme for the app (no-op on iOS).
    ///
    /// On iOS, URL schemes must be declared in the app's Info.plist; the provided `url` is ignored.
    ///
    /// # Parameters
    ///
    /// - `url`: URL scheme to register (ignored on iOS).
    ///
    /// # Returns
    ///
    /// `Ok(())` indicating registration is either unnecessary or assumed already configured.
    ///
    /// # Examples
    ///
    /// ```
    /// let _ = register_url_scheme("myapp");
    /// ```
    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        // URL schemes are registered in Info.plist on iOS, not programmatically
        Task::ready(Ok(()))
    }

    /// Prompt the user to select one or more local filesystem paths.
    ///
    /// This implementation always results in an error on iOS: local file picking is not supported
    /// by this application and the returned receiver will yield an `Err` explaining that local
    /// file access is unavailable.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use futures::executor::block_on;
    /// use gpui::platform::ios::IosPlatform;
    /// use gpui::platform::PathPromptOptions;
    ///
    /// let platform = IosPlatform::new();
    /// let rx = platform.prompt_for_paths(PathPromptOptions::default());
    /// let result = block_on(rx).expect("oneshot channel closed");
    /// assert!(result.is_err());
    /// ```
    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        // iOS uses UIDocumentPickerViewController for file access
        // For a remote-first app, we don't support local file picking
        tx.send(Err(anyhow!("Local file access not supported on iOS. Use remote connection."))).ok();
        rx
    }

    /// Requests a new file path from the user; on iOS this operation is not supported and immediately fails.
    ///
    /// This function returns a oneshot receiver that yields an error indicating local file access is not supported on iOS.
    ///
    /// # Examples
    ///
    /// ```
    /// use futures::executor::block_on;
    /// use futures::channel::oneshot;
    /// use anyhow::Result;
    /// use std::path::PathBuf;
    ///
    /// // `rx` is the oneshot::Receiver returned by `prompt_for_new_path`.
    /// // In real use, obtain `rx` by calling `prompt_for_new_path` on the platform instance.
    /// let (tx, rx): (oneshot::Sender<Result<Option<PathBuf>>>, oneshot::Receiver<Result<Option<PathBuf>>>) = oneshot::channel();
    /// // Simulate the platform behavior by sending an error as iOS does:
    /// tx.send(Err(anyhow::anyhow!("Local file access not supported on iOS. Use remote connection."))).ok();
    /// let received = block_on(rx).unwrap();
    /// assert!(received.is_err());
    /// ```
    fn prompt_for_new_path(&self, _directory: &Path, _suggested_name: Option<&str>) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!("Local file access not supported on iOS. Use remote connection."))).ok();
        rx
    }

    /// Indicates whether the platform allows selecting files and directories in the same file-selection prompt.
    ///
    /// # Returns
    ///
    /// `false` (iOS does not support selecting files and directories together).
    ///
    /// # Examples
    ///
    /// ```
    /// let p = IosPlatform::new();
    /// assert!(!p.can_select_mixed_files_and_dirs());
    /// ```
    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    /// No-op placeholder for revealing a filesystem path in the system UI.
    ///
    /// On iOS there is no equivalent to macOS Finder's "reveal in Finder"; this method intentionally does nothing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    ///
    /// // On iOS this call is a no-op.
    /// let platform = /* IosPlatform instance */;
    /// platform.reveal_path(Path::new("/path/to/file"));
    /// ```
    fn reveal_path(&self, _path: &Path) {
        // Not supported on iOS in the same way as macOS Finder
    }

    /// No-op on iOS that ignores the provided filesystem path.
    ///
    /// On iOS this operation is not supported; the method performs no action and the `path` is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// // Calling on an `IosPlatform` has no effect.
    /// let platform = IosPlatform::new();
    /// platform.open_with_system(Path::new("/some/path"));
    /// ```
    fn open_with_system(&self, _path: &Path) {
        // Could use UIDocumentInteractionController, but not needed for remote-first
    }

    /// Registers a callback to be invoked when the platform requests application quit.
    ///
    /// The provided `callback` will replace any previously-registered quit handler and will be
    /// invoked (by the platform implementation) when a quit event occurs.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::atomic::{AtomicBool, Ordering};
    /// use std::sync::Arc;
    ///
    /// let called = Arc::new(AtomicBool::new(false));
    /// let called_clone = called.clone();
    /// platform.on_quit(Box::new(move || {
    ///     called_clone.store(true, Ordering::SeqCst);
    /// }));
    /// // Later, when the platform triggers quit, `called` will be set to true.
    /// ```
    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().quit_callback = Some(callback);
    }

    /// Registers a callback to be invoked when the application is reopened.
    ///
    /// On iOS this operation does nothing because the platform does not have a concept
    /// of "reopen" (e.g., macOS dock-click reopen); the provided callback is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// // Calling on_reopen on iOS has no effect, but the call is valid.
    /// let platform = /* IosPlatform instance */;
    /// platform.on_reopen(Box::new(|| {
    ///     // will not be called on iOS
    /// }));
    /// ```
    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {
        // iOS apps don't have a "reopen" concept like macOS dock click
    }

    /// Registers a callback to be invoked when the system keyboard layout changes.
    ///
    /// On iOS this function is a no-op: the provided callback will not be stored or called.
    ///
    /// # Examples
    ///
    /// ```
    /// // Passing a closure is accepted but on iOS it will not be invoked.
    /// platform.on_keyboard_layout_change(Box::new(|| {
    ///     // Respond to a layout change
    /// }));
    /// ```
    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {
        // Could observe UITextInputMode.currentInputModeDidChangeNotification
        // but not critical for initial implementation
    }

    /// No-op on iOS — menus and key mappings are not applicable.
    ///
    /// The provided `menus` and `keymap` arguments are ignored. This exists as a
    /// platform-specific stub; future integrations could map menus to `UIMenuSystem`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // On iOS this call does nothing.
    /// platform.set_menus(vec![], &keymap);
    /// ```
    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {
        // iOS doesn't have a menu bar
        // Could potentially use UIMenuSystem for context menus in future
    }

    /// No-op placeholder for setting a dock menu on iOS.
    ///
    /// iOS does not support dock menus; calling this has no effect.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// // iOS ignores dock menus
    /// let platform = /* IosPlatform instance */;
    /// platform.set_dock_menu(vec![], &/* keymap */);
    /// ```
    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // iOS doesn't have a dock menu
    }

    /// Registers a callback for application menu actions — ignored on iOS.
    ///
    /// The iOS platform does not provide an application menu; the supplied callback
    /// will not be stored or invoked.
    ///
    /// # Examples
    ///
    /// ```
    /// let mut cb = |_: &dyn Action| {};
    /// // On iOS this is a no-op:
    /// // platform.on_app_menu_action(Box::new(cb));
    /// ```
    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {
        // No app menu on iOS
    }

    /// Registers a callback to be invoked just before the application menu opens on platforms that provide an app menu.
    ///
    /// On iOS there is no application menu, so the provided callback is ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// // Create a callback and pass it to the platform; on iOS this has no effect.
    /// let mut cb = Box::new(|| println!("app menu will open"));
    /// // `platform` would be an instance of the platform implementation:
    /// // platform.on_will_open_app_menu(cb);
    /// ```
    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {
        // No app menu on iOS
    }

    /// Registers a validator for application menu commands; ignored on iOS.
    ///
    /// This method accepts a callback intended to validate app menu `Action`s but is a no-op on iOS
    /// because the platform does not expose a traditional application menu.
    ///
    /// # Parameters
    ///
    /// * `callback` - A closure that would be used to validate menu actions on platforms with an app menu.
    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        // No app menu on iOS
    }

    /// Creates a keyboard layout object representing the current iOS input mode.
    ///
    /// The returned object implements `PlatformKeyboardLayout` and exposes the layout's
    /// identifier and human-readable name.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let layout = platform.keyboard_layout();
    /// assert!(!layout.id().is_empty());
    /// assert!(!layout.name().is_empty());
    /// ```
    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(IosKeyboardLayout::new())
    }

    /// Provide a platform keyboard mapper for iOS.
    ///
    /// The returned value is an `Rc`-wrapped `PlatformKeyboardMapper`. On iOS this is a placeholder
    /// implementation (a `DummyKeyboardMapper`) that satisfies the platform API.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// let _mapper = platform.keyboard_mapper();
    /// ```
    fn
    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    /// Returns the filesystem path to the application's bundle.
    ///
    /// # Returns
    ///
    /// `Ok(PathBuf)` containing the app bundle path on success, `Err` if the bundle path cannot be retrieved or is not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```
    /// // Given an `IosPlatform` instance `platform`:
    /// // let path = platform.app_path().expect("failed to get app path");
    /// // assert!(path.is_absolute());
    /// ```
    fn app_path(&self) -> Result<PathBuf> {
        unsafe {
            let bundle: *mut Object = msg_send![class!(NSBundle), mainBundle];
            let path: *mut Object = msg_send![bundle, bundlePath];
            let utf8: *const i8 = msg_send![path, UTF8String];
            if utf8.is_null() {
                Err(anyhow!("Failed to get app path"))
            } else {
                Ok(PathBuf::from(
                    std::ffi::CStr::from_ptr(utf8).to_str()?,
                ))
            }
        }
    }

    /// Attempt to resolve the path of an auxiliary executable (unsupported on iOS).
    ///
    /// This platform does not provide auxiliary executables; the `name` parameter is ignored
    /// and the function always returns an error indicating the operation is unsupported.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// assert!(platform.path_for_auxiliary_executable("helper").is_err());
    /// ```
    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        // iOS apps don't have auxiliary executables in the same way
        Err(anyhow!("Auxiliary executables not supported on iOS"))
    }

    /// Sets the cursor style for the platform.
    ///
    /// On iOS this has no visible effect because the platform does not expose a system cursor; the call is a no-op.
    /// May be extended in the future to configure pointer styles for trackpad/mouse using UIPointerStyle.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// let platform = /* obtain IosPlatform instance */;
    /// platform.set_cursor_style(crate::CursorStyle::Default);
    /// ```
    fn set_cursor_style(&self, _style: crate::CursorStyle) {
        // iOS doesn't have a visible cursor (except for pointer on iPad with trackpad)
        // Could implement with UIPointerStyle for trackpad support in future
    }

    /// Indicates whether scrollbars should automatically hide on iOS.
    ///
    /// # Returns
    ///
    /// `true` if scrollbars should auto-hide, `false` otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// let platform = IosPlatform::new();
    /// assert!(platform.should_auto_hide_scrollbars());
    /// ```
    fn should_auto_hide_scrollbars(&self) -> bool {
        true // iOS always auto-hides scrollbars
    }

    /// Writes clipboard entries to the iOS general pasteboard.
    ///
    /// Only `String` entries from `item` are written to `UIPasteboard::generalPasteboard`. Image and
    /// external-path entries are ignored on iOS.
    ///
    /// # Examples
    ///
    /// ```
    /// // Create a clipboard item containing a single string and write it to the system pasteboard.
    /// let item = ClipboardItem::new_string("Hello, world!".into());
    /// platform.write_to_clipboard(item);
    /// ```
    fn write_to_clipboard(&self, item: ClipboardItem) {
        unsafe {
            let pasteboard: *mut Object = msg_send![class!(UIPasteboard), generalPasteboard];

            for entry in item.entries() {
                match entry {
                    ClipboardEntry::String(s) => {
                        let ns_string = ns_string(s.text());
                        let _: () = msg_send![pasteboard, setString: ns_string];
                    }
                    ClipboardEntry::Image(_img) => {
                        // Could implement image clipboard support
                        // using setImage: or setData:forPasteboardType:
                    }
                    ClipboardEntry::ExternalPaths(_paths) => {
                        // External file paths are not typically supported on iOS clipboard
                    }
                }
            }
        }
    }

    /// Reads a UTF-8 text value from the system clipboard and returns it wrapped as a `ClipboardItem`.
    ///
    /// If the clipboard contains a string that can be decoded as UTF-8, the string is returned inside
    /// `Some(ClipboardItem)`. If the clipboard is empty, contains non-string data, or the string is
    /// not valid UTF-8, `None` is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// if let Some(item) = read_from_clipboard() {
    ///     // inspect or use `item` (e.g., extract the string if your `ClipboardItem` API provides that)
    ///     let _ = item;
    /// }
    /// ```
    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        unsafe {
            let pasteboard: *mut Object = msg_send![class!(UIPasteboard), generalPasteboard];
            let string: *mut Object = msg_send![pasteboard, string];
            if !string.is_null() {
                let utf8: *const i8 = msg_send![string, UTF8String];
                if !utf8.is_null() {
                    let s = std::ffi::CStr::from_ptr(utf8)
                        .to_str()
                        .ok()?
                        .to_string();
                    return Some(ClipboardItem::new_string(s));
                }
            }
            None
        }
    }

    /// Attempts to store credentials (service URL, username, password) in the iOS Keychain.
    ///
    /// Currently this is not implemented on iOS; the returned task resolves to an error
    /// indicating that Keychain write support is not yet provided.
    ///
    /// # Examples
    ///
    /// ```
    /// // let res = tokio::runtime::Runtime::new().unwrap()
    /// //     .block_on(platform.write_credentials("https://example.com", "user", b"pass").await);
    /// // assert!(res.is_err());
    /// ```
    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        // TODO: Implement iOS Keychain support properly using the Security framework.
        // The correct implementation requires:
        // 1. Import Security framework CFString constants (kSecClass, kSecAttrService,
        //    kSecAttrAccount, kSecValueData, etc.) rather than using string literals
        //    like ns_string("kSecAttrService") which won't match the actual constants.
        // 2. Use SecItemAdd for new entries and SecItemUpdate for existing ones.
        // 3. Build a proper CFDictionary with the imported constant keys.
        // 4. Handle the OSStatus return values from Security framework functions.
        self.background_executor().spawn(async move {
            Err(anyhow!("Keychain write_credentials not yet implemented for iOS"))
        })
    }

    /// Attempts to read credentials for the given URL from the iOS Keychain.
    ///
    /// Currently unimplemented on iOS and always returns an error indicating Keychain
    /// read is not available.
    ///
    /// # Returns
    ///
    /// `Ok(Some((username, password_bytes)))` if credentials were found, `Ok(None)` if no
    /// credentials exist for the URL, or `Err(_)` with an error explaining that Keychain
    /// read is not yet implemented on iOS.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use anyhow::Result;
    /// # use futures::executor::block_on;
    /// // let platform: IosPlatform = ...;
    /// // let task = platform.read_credentials("https://example.com");
    /// // let result = block_on(task);
    /// // assert!(result.is_err());
    /// ```
    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        // TODO: Implement iOS Keychain support properly using the Security framework.
        // See write_credentials for details on the correct implementation approach.
        self.background_executor().spawn(async move {
            Err(anyhow!("Keychain read_credentials not yet implemented for iOS"))
        })
    }

    /// Deletes stored credentials associated with the given URL from the platform credential store.
    ///
    /// `url` is the identifier for the credentials to remove (typically an origin or service URL).
    /// Returns a `Task` that resolves to `Ok(())` on success or an `Err` on failure; currently this function
    /// always resolves to an `Err` indicating Keychain support is not yet implemented on iOS.
    ///
    /// # Examples
    ///
    /// ```rust
    /// // Pseudocode usage — `platform` is an instance implementing the platform API.
    /// // The returned Task can be `.await`ed in an async context.
    /// // let result = platform.delete_credentials("https://example.com").await;
    /// // assert!(result.is_err());
    /// ```
    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        // TODO: Implement iOS Keychain support properly using the Security framework.
        // See write_credentials for details on the correct implementation approach.
        self.background_executor().spawn(async move {
            Err(anyhow!("Keychain delete_credentials not yet implemented for iOS"))
        })
    }
}

/// iOS keyboard layout information.
pub struct IosKeyboardLayout {
    id: String,
    name: String,
}

impl IosKeyboardLayout {
    /// Creates an `IosKeyboardLayout` by querying the system's current text input mode.
    ///
    /// Queries `UITextInputMode.currentInputMode` to obtain the `primaryLanguage` and uses its UTF-8
    /// representation to populate the layout `id` and `name`. If the current input mode or language
    /// cannot be determined, defaults to an `id` of `"en"` and a `name` of `"English"`.
    ///
    /// # Examples
    ///
    /// ```
    /// let layout = IosKeyboardLayout::new();
    /// assert!(!layout.id().is_empty());
    /// assert!(!layout.name().is_empty());
    /// ```
    pub fn new() -> Self {
        unsafe {
            // Get current input mode
            let text_input_mode: *mut Object = msg_send![class!(UITextInputMode), currentInputMode];

            let (id, name) = if !text_input_mode.is_null() {
                let primary_language: *mut Object = msg_send![text_input_mode, primaryLanguage];
                if !primary_language.is_null() {
                    let utf8: *const i8 = msg_send![primary_language, UTF8String];
                    if !utf8.is_null() {
                        let lang = std::ffi::CStr::from_ptr(utf8)
                            .to_str()
                            .unwrap_or("en")
                            .to_string();
                        (lang.clone(), lang)
                    } else {
                        ("en".to_string(), "English".to_string())
                    }
                } else {
                    ("en".to_string(), "English".to_string())
                }
            } else {
                ("en".to_string(), "English".to_string())
            };

            Self { id, name }
        }
    }
}

impl PlatformKeyboardLayout for IosKeyboardLayout {
    /// Get the keyboard layout identifier.
    ///
    /// # Returns
    ///
    /// The layout identifier as a `&str`.
    ///
    /// # Examples
    ///
    /// ```
    /// let layout = gpui::platform::ios::platform::IosKeyboardLayout::new();
    /// let id = layout.id();
    /// assert!(!id.is_empty());
    /// ```
    fn id(&self) -> &str {
        &self.id
    }

    /// The human-readable display name for this keyboard layout.
    ///
    /// # Returns
    ///
    /// The layout's name (for example, `"English"`).
    ///
    /// # Examples
    ///
    /// ```
    /// let layout = IosKeyboardLayout::new();
    /// let name = layout.name();
    /// assert!(!name.is_empty());
    /// ```
    fn name(&self) -> &str {
        &self.name
    }
}