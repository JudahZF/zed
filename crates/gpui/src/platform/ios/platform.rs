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
    pub fn new() -> Self {
        let dispatcher = Arc::new(IosDispatcher::new());

        #[cfg(feature = "font-kit")]
        let text_system = Arc::new(super::MacTextSystem::new()) as Arc<dyn PlatformTextSystem>;

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

    /// Called when the app receives URLs to open.
    pub fn handle_open_urls(&self, urls: Vec<String>) {
        let mut state = self.0.lock();
        if let Some(callback) = state.open_urls.as_mut() {
            callback(urls);
        }
    }

    /// Called when the app is about to terminate.
    pub fn will_terminate(&self) {
        let mut state = self.0.lock();
        if let Some(callback) = state.quit_callback.as_mut() {
            callback();
        }
    }

    fn renderer_context(&self) -> renderer::Context {
        self.0.lock().renderer_context.clone()
    }
}

impl Platform for IosPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.0.lock().background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.0.lock().foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.0.lock().text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        // On iOS, UIApplicationMain owns and manages the main run loop.
        // Unlike macOS where we need to explicitly run NSRunLoop, on iOS
        // the run loop is already running by the time this is called.
        // We only need to invoke the finish launching callback and return
        // immediately to avoid blocking the UIKit event handling.
        on_finish_launching();
    }

    fn quit(&self) {
        // iOS apps don't quit programmatically in the traditional sense.
        // The system manages app lifecycle.
        // We can notify the callback though.
        self.will_terminate();
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        // Not supported on iOS - apps cannot restart themselves
    }

    fn activate(&self, _ignoring_other_apps: bool) {
        // iOS apps are activated by the system, not programmatically
    }

    fn hide(&self) {
        // iOS doesn't have a "hide" concept like macOS
    }

    fn hide_other_apps(&self) {
        // Not applicable on iOS
    }

    fn unhide_other_apps(&self) {
        // Not applicable on iOS
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        IosDisplay::all()
            .into_iter()
            .map(|d| Rc::new(d) as Rc<dyn PlatformDisplay>)
            .collect()
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay::main()))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        let state = self.0.lock();
        state.windows.first().copied()
    }

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

    fn open_url(&self, url: &str) {
        unsafe {
            let url_string = ns_string(url);
            let nsurl: *mut Object = msg_send![class!(NSURL), URLWithString: url_string];
            let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
            let _: () = msg_send![app, openURL: nsurl options: std::ptr::null::<Object>() completionHandler: std::ptr::null::<c_void>()];
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.0.lock().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        // URL schemes are registered in Info.plist on iOS, not programmatically
        Task::ready(Ok(()))
    }

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

    fn prompt_for_new_path(&self, _directory: &Path, _suggested_name: Option<&str>) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!("Local file access not supported on iOS. Use remote connection."))).ok();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {
        // Not supported on iOS in the same way as macOS Finder
    }

    fn open_with_system(&self, _path: &Path) {
        // Could use UIDocumentInteractionController, but not needed for remote-first
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().quit_callback = Some(callback);
    }

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {
        // iOS apps don't have a "reopen" concept like macOS dock click
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {
        // Could observe UITextInputMode.currentInputModeDidChangeNotification
        // but not critical for initial implementation
    }

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {
        // iOS doesn't have a menu bar
        // Could potentially use UIMenuSystem for context menus in future
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        // iOS doesn't have a dock menu
    }

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {
        // No app menu on iOS
    }

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {
        // No app menu on iOS
    }

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        // No app menu on iOS
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(IosKeyboardLayout::new())
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

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

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        // iOS apps don't have auxiliary executables in the same way
        Err(anyhow!("Auxiliary executables not supported on iOS"))
    }

    fn set_cursor_style(&self, _style: crate::CursorStyle) {
        // iOS doesn't have a visible cursor (except for pointer on iPad with trackpad)
        // Could implement with UIPointerStyle for trackpad support in future
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true // iOS always auto-hides scrollbars
    }

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

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        unsafe {
            let pasteboard: *mut Object = msg_send![class!(UIPasteboard), generalPasteboard];
            let has_strings: bool = msg_send![pasteboard, hasStrings];

            if has_strings {
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
            }
            None
        }
    }

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

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        // TODO: Implement iOS Keychain support properly using the Security framework.
        // See write_credentials for details on the correct implementation approach.
        self.background_executor().spawn(async move {
            Err(anyhow!("Keychain read_credentials not yet implemented for iOS"))
        })
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        // TODO: Implement iOS Keychain support properly using the Security framework.
        // See write_credentials for details on the correct implementation approach.
        self.background_executor().spawn(async move {
            Err(anyhow!("Keychain delete_credentials not yet implemented for iOS"))
        })
    }

    fn is_screen_capture_supported(&self) -> bool {
        // Screen capture is not supported on iOS in the same way as desktop platforms.
        // iOS uses ReplayKit which has different privacy/permission requirements.
        false
    }

    fn screen_capture_sources(
        &self,
    ) -> oneshot::Receiver<Result<Vec<Rc<dyn crate::ScreenCaptureSource>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!("Screen capture not supported on iOS")))
            .ok();
        rx
    }
}

/// iOS keyboard layout information.
pub struct IosKeyboardLayout {
    id: String,
    name: String,
}

impl IosKeyboardLayout {
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
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }
}
