//! Zed for iPadOS - Remote-first code editor
//!
//! This crate provides the iOS application entry point for Zed,
//! a thin client that connects to remote development machines.

mod welcome_view;

use anyhow::Result;
use gpui::{App, AppContext as _, Application, WindowOptions};
use welcome_view::WelcomeView;

/// Log to iOS - we'll write to a file in the app's temp directory for inspection
fn ios_log(message: &str) {
    // Write to stderr which should be captured somewhere
    eprintln!("[Zed Rust] {}", message);
    
    // Also write to a log file in temp directory
    use std::io::Write;
    let mut log_path = std::env::temp_dir();
    log_path.push("zed_ios.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(file, "[Zed Rust] {}", message);
    }
}

/// Main entry point for the iOS application.
/// Called from the Objective-C app delegate when the app finishes launching.
pub struct ZedIosApp;

impl ZedIosApp {
    /// Initialize the iOS application.
    /// This sets up the minimal required infrastructure and opens the main window.
    pub fn init(cx: &mut App) -> Result<()> {
        ios_log("ZedIosApp::init starting");
        
        // Load embedded assets (fonts, icons, etc.)
        ios_log("Loading fonts...");
        
        // List what fonts we have
        if let Ok(font_paths) = cx.asset_source().list("fonts") {
            ios_log(&format!("Found {} font paths", font_paths.len()));
            for path in font_paths.iter().take(5) {
                ios_log(&format!("  - {}", path));
            }
        }
        
        assets::Assets
            .load_fonts(cx)
            .map_err(|e| anyhow::anyhow!("Failed to load fonts: {}", e))?;
        ios_log("Fonts loaded");
        
        // List loaded font names
        let font_names = cx.text_system().all_font_names();
        ios_log(&format!("{} fonts available in text system", font_names.len()));
        for name in font_names.iter().take(10) {
            ios_log(&format!("  - {}", name));
        }

        // Initialize settings with defaults
        ios_log("Initializing settings...");
        settings::init(cx);
        ios_log("Settings initialized");

        // Initialize theme system with base themes only
        ios_log("Initializing theme...");
        theme::init(theme::LoadThemes::JustBase, cx);
        ios_log("Theme initialized");

        // Open the main window with welcome view
        ios_log("Opening window...");
        cx.open_window(
            WindowOptions {
                titlebar: None,
                focus: true,
                show: true,
                ..Default::default()
            },
            |_, cx| {
                ios_log("Creating WelcomeView...");
                cx.new(|_cx| WelcomeView::new())
            },
        )?;
        ios_log("Window opened successfully");

        Ok(())
    }
}

/// Entry point called from Objective-C app delegate.
/// This function is marked #[no_mangle] so it can be called by name from main.m
#[unsafe(no_mangle)]
pub extern "C" fn zed_ios_init() {
    ios_log("zed_ios_init called");
    
    // Try to initialize logging - ignore errors since stderr might work anyway
    let _ = env_logger::try_init();
    
    ios_log("Creating Application...");
    Application::new()
        .with_assets(assets::Assets)
        .run(|cx| {
            ios_log("Application::run callback executing");
            if let Err(e) = ZedIosApp::init(cx) {
                ios_log(&format!("Failed to initialize Zed iOS: {}", e));
            }
            ios_log("Application::run callback finished");
        });
    ios_log("Application::run returned");
}
