//! iOS platform implementation.
//!
//! This module provides the main platform interface for iOS, implementing the
//! `Platform` trait with iOS-specific functionality.

use super::{IosDispatcher, IosDisplay, IosWindow, ns_string, renderer};
use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardEntry, ClipboardItem,
    DummyKeyboardMapper, ForegroundExecutor, IosLifecycleEvent, Keymap, Menu, MenuItem,
    PathPromptOptions, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, Result, Subscription, Task, ThermalState, WindowAppearance,
    WindowParams,
};
use anyhow::{Context as _, anyhow};
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::{CFDictionary, CFMutableDictionary},
    string::CFString,
};
use core_foundation_sys::{base::OSStatus, dictionary::CFDictionaryRef, string::CFStringRef};
use futures::channel::oneshot;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use std::{
    ffi::CStr,
    ffi::c_void,
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::{Arc, OnceLock},
};

pub struct IosPlatform(Mutex<IosPlatformState>);

type PathPromptResult = Result<Option<Vec<PathBuf>>>;
type PathPromptSender = oneshot::Sender<PathPromptResult>;

static ACTIVE_PATH_PROMPT: Mutex<Option<PathPromptSender>> = Mutex::new(None);
static ACTIVE_PATH_PROMPT_DELEGATE: Mutex<Option<usize>> = Mutex::new(None);
static DOCUMENT_PICKER_DELEGATE_CLASS: OnceLock<&'static Class> = OnceLock::new();

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

fn ensure_document_picker_delegate_registered() {
    DOCUMENT_PICKER_DELEGATE_CLASS
        .get_or_init(|| unsafe { register_document_picker_delegate_class() });
}

unsafe fn register_document_picker_delegate_class() -> &'static Class {
    let superclass = class!(NSObject);
    let mut decl = ClassDecl::new("GPUIDocumentPickerDelegate", superclass).unwrap();
    unsafe {
        decl.add_method(
            sel!(documentPicker:didPickDocumentsAtURLs:),
            document_picker_did_pick_documents
                as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
        );
        decl.add_method(
            sel!(documentPicker:didPickDocumentAtURL:),
            document_picker_did_pick_document
                as extern "C" fn(&Object, Sel, *mut Object, *mut Object),
        );
        decl.add_method(
            sel!(documentPickerWasCancelled:),
            document_picker_was_cancelled as extern "C" fn(&Object, Sel, *mut Object),
        );
    }
    decl.register()
}

extern "C" fn document_picker_did_pick_documents(
    _this: &Object,
    _sel: Sel,
    _picker: *mut Object,
    urls: *mut Object,
) {
    unsafe {
        finish_active_path_prompt(ns_urls_to_paths(urls).map(Some));
    }
}

extern "C" fn document_picker_did_pick_document(
    _this: &Object,
    _sel: Sel,
    _picker: *mut Object,
    url: *mut Object,
) {
    unsafe {
        finish_active_path_prompt(ns_url_to_path(url).map(|path| Some(vec![path])));
    }
}

extern "C" fn document_picker_was_cancelled(_this: &Object, _sel: Sel, _picker: *mut Object) {
    finish_active_path_prompt(Ok(None));
}

fn finish_active_path_prompt(result: PathPromptResult) {
    if let Some(sender) = ACTIVE_PATH_PROMPT.lock().take() {
        sender.send(result).ok();
    }
    release_active_path_prompt_delegate();
}

fn release_active_path_prompt_delegate() {
    let delegate = ACTIVE_PATH_PROMPT_DELEGATE.lock().take();
    if let Some(delegate) = delegate {
        unsafe {
            let delegate = delegate as *mut Object;
            let _: () = msg_send![delegate, release];
        }
    }
}

unsafe fn ns_url_to_path(url: *mut Object) -> Result<PathBuf> {
    if url.is_null() {
        return Err(anyhow!("Document picker returned a null URL"));
    }

    let is_file_url: BOOL = msg_send![url, isFileURL];
    if is_file_url == NO {
        return Err(anyhow!("Document picker returned a non-file URL"));
    }

    let path: *mut Object = msg_send![url, path];
    if path.is_null() {
        return Err(anyhow!("Document picker returned a URL without a path"));
    }

    let utf8: *const i8 = msg_send![path, UTF8String];
    if utf8.is_null() {
        return Err(anyhow!(
            "Document picker returned a path that was not UTF-8"
        ));
    }

    let path = unsafe { CStr::from_ptr(utf8) }.to_str()?;
    Ok(PathBuf::from(path))
}

unsafe fn ns_urls_to_paths(urls: *mut Object) -> Result<Vec<PathBuf>> {
    if urls.is_null() {
        return Err(anyhow!("Document picker returned no URLs"));
    }

    let count: usize = msg_send![urls, count];
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let url: *mut Object = msg_send![urls, objectAtIndex: index];
        paths.push(unsafe { ns_url_to_path(url) }?);
    }
    Ok(paths)
}

unsafe fn active_presenting_view_controller() -> *mut Object {
    let app: *mut Object = msg_send![class!(UIApplication), sharedApplication];
    let mut key_window: *mut Object = msg_send![app, keyWindow];

    if key_window.is_null() {
        let windows: *mut Object = msg_send![app, windows];
        if !windows.is_null() {
            let count: usize = msg_send![windows, count];
            for index in 0..count {
                let candidate: *mut Object = msg_send![windows, objectAtIndex: index];
                if key_window.is_null() {
                    key_window = candidate;
                }

                let is_key_window: BOOL = msg_send![candidate, isKeyWindow];
                if is_key_window == YES {
                    key_window = candidate;
                    break;
                }
            }
        }
    }

    if key_window.is_null() {
        return ptr::null_mut();
    }

    let mut view_controller: *mut Object = msg_send![key_window, rootViewController];
    while !view_controller.is_null() {
        let presented: *mut Object = msg_send![view_controller, presentedViewController];
        if presented.is_null() {
            break;
        }
        view_controller = presented;
    }

    view_controller
}

pub fn observe_ios_lifecycle(callback: impl FnMut(IosLifecycleEvent) + 'static) -> Subscription {
    super::ffi::observe_ios_lifecycle(callback)
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
        // immediately to avoid blocking UIKit event handling. The GPUI app
        // lifetime itself is retained separately by the iOS app runtime.
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
                2 => WindowAppearance::Dark,  // UIUserInterfaceStyleDark
                _ => WindowAppearance::Light, // UIUserInterfaceStyleLight or Unspecified
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
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        if options.directories {
            tx.send(Err(anyhow!("Directory selection is not supported on iOS.")))
                .ok();
            return rx;
        }
        if !options.files {
            tx.send(Err(anyhow!(
                "The iOS file picker only supports file selection."
            )))
            .ok();
            return rx;
        }
        if ACTIVE_PATH_PROMPT.lock().is_some() {
            tx.send(Err(anyhow!("Another iOS file picker is already active.")))
                .ok();
            return rx;
        }

        *ACTIVE_PATH_PROMPT.lock() = Some(tx);
        self.foreground_executor()
            .spawn(async move {
                unsafe {
                    ensure_document_picker_delegate_registered();

                    let document_types: *mut Object = msg_send![class!(NSMutableArray), array];
                    let public_data = ns_string("public.data");
                    let public_text = ns_string("public.text");
                    let _: () = msg_send![document_types, addObject: public_data];
                    let _: () = msg_send![document_types, addObject: public_text];

                    let picker: *mut Object =
                        msg_send![class!(UIDocumentPickerViewController), alloc];
                    let picker: *mut Object =
                        msg_send![picker, initWithDocumentTypes: document_types inMode: 0usize];

                    if picker.is_null() {
                        finish_active_path_prompt(Err(anyhow!(
                            "Failed to create the iOS document picker."
                        )));
                        return;
                    }

                    if let Some(prompt) = options.prompt.as_ref() {
                        let title = ns_string(prompt);
                        let _: () = msg_send![picker, setTitle: title];
                    }

                    let delegate_class = *DOCUMENT_PICKER_DELEGATE_CLASS.get().unwrap();
                    let delegate: *mut Object = msg_send![delegate_class, new];
                    *ACTIVE_PATH_PROMPT_DELEGATE.lock() = Some(delegate as usize);

                    let _: () = msg_send![picker, setDelegate: delegate];
                    let _: () = msg_send![
                        picker,
                        setAllowsMultipleSelection: if options.multiple { YES } else { NO }
                    ];

                    let presenter = active_presenting_view_controller();
                    if presenter.is_null() {
                        finish_active_path_prompt(Err(anyhow!(
                            "No active iOS view controller was available to present the file picker."
                        )));
                        return;
                    }

                    let _: () = msg_send![
                        presenter,
                        presentViewController: picker
                        animated: YES
                        completion: ptr::null::<c_void>()
                    ];
                }
            })
            .detach();
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!(
            "Local file access not supported on iOS. Use remote connection."
        )))
        .ok();
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

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        drop(callback);
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
                Ok(PathBuf::from(std::ffi::CStr::from_ptr(utf8).to_str()?))
            }
        }
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
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
                        let s = std::ffi::CStr::from_ptr(utf8).to_str().ok()?.to_string();
                        return Some(ClipboardItem::new_string(s));
                    }
                }
            }
            None
        }
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let url = url.to_string();
        let username = username.to_string();
        let password = password.to_vec();

        self.background_executor().spawn(async move {
            unsafe {
                use security::*;

                let url = CFString::from(url.as_str());
                let username = CFString::from(username.as_str());
                let password = CFData::from_buffer(&password);

                let mut verb = "updating";
                let mut query_attrs = CFMutableDictionary::with_capacity(2);
                query_attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                query_attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());

                let mut attrs = CFMutableDictionary::with_capacity(4);
                attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());
                attrs.set(kSecAttrAccount as *const _, username.as_CFTypeRef());
                attrs.set(kSecValueData as *const _, password.as_CFTypeRef());

                let mut status = SecItemUpdate(
                    query_attrs.as_concrete_TypeRef(),
                    attrs.as_concrete_TypeRef(),
                );

                if status == errSecItemNotFound {
                    verb = "creating";
                    status = SecItemAdd(attrs.as_concrete_TypeRef(), ptr::null_mut());
                }

                anyhow::ensure!(status == errSecSuccess, "{verb} password failed: {status}");
            }

            Ok(())
        })
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let url = url.to_string();

        self.background_executor().spawn(async move {
            let url = CFString::from(url.as_str());
            let cf_true = CFBoolean::true_value().as_CFTypeRef();

            unsafe {
                use security::*;

                let mut attrs = CFMutableDictionary::with_capacity(5);
                attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());
                attrs.set(kSecReturnAttributes as *const _, cf_true);
                attrs.set(kSecReturnData as *const _, cf_true);

                let mut result = CFTypeRef::from(ptr::null());
                let status = SecItemCopyMatching(attrs.as_concrete_TypeRef(), &mut result);
                if status == errSecItemNotFound || status == errSecUserCanceled {
                    return Ok(None);
                }
                if status != errSecSuccess {
                    anyhow::bail!("reading password failed: {status}");
                }

                let result = CFType::wrap_under_create_rule(result)
                    .downcast::<CFDictionary>()
                    .context("keychain item was not a dictionary")?;
                let username = result
                    .find(kSecAttrAccount as *const _)
                    .context("account was missing from keychain item")?;
                let username = CFType::wrap_under_get_rule(*username)
                    .downcast::<CFString>()
                    .context("account was not a string")?;
                let password = result
                    .find(kSecValueData as *const _)
                    .context("password was missing from keychain item")?;
                let password = CFType::wrap_under_get_rule(*password)
                    .downcast::<CFData>()
                    .context("password was not data")?;

                Ok(Some((username.to_string(), password.bytes().to_vec())))
            }
        })
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let url = url.to_string();

        self.background_executor().spawn(async move {
            unsafe {
                use security::*;

                let url = CFString::from(url.as_str());
                let mut query_attrs = CFMutableDictionary::with_capacity(2);
                query_attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                query_attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());

                let status = SecItemDelete(query_attrs.as_concrete_TypeRef());
                anyhow::ensure!(
                    status == errSecSuccess || status == errSecItemNotFound,
                    "delete password failed: {status}"
                );
            }

            Ok(())
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

mod security {
    #![allow(non_upper_case_globals)]
    use super::*;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        pub static kSecClass: CFStringRef;
        pub static kSecClassInternetPassword: CFStringRef;
        pub static kSecAttrServer: CFStringRef;
        pub static kSecAttrAccount: CFStringRef;
        pub static kSecValueData: CFStringRef;
        pub static kSecReturnAttributes: CFStringRef;
        pub static kSecReturnData: CFStringRef;

        pub fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        pub fn SecItemUpdate(query: CFDictionaryRef, attributes: CFDictionaryRef) -> OSStatus;
        pub fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
        pub fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
    }

    pub const errSecSuccess: OSStatus = 0;
    pub const errSecUserCanceled: OSStatus = -128;
    pub const errSecItemNotFound: OSStatus = -25300;
}
