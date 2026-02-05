//! Zed for iPadOS - Remote-first code editor
//!
//! This crate provides the iOS application entry point for Zed,
//! a thin client that connects to remote development machines.

mod connect_view;
mod persistence;
mod remote_delegate;
mod root_view;
mod text_input;
mod tutorial_view;
mod welcome_view;
mod workspace_view;

use anyhow::Result;
use client::{Client, UserStore};
use fs::{Fs, RealFs};
use gpui::{App, AppContext as _, Application, WindowOptions};
use language::LanguageRegistry;
use log::info;
use paths::{log_file, logs_dir, old_log_file};
use project::Project;
use root_view::RootView;
use session::{AppSession, Session};
use std::sync::Arc;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use uuid::Uuid;
use workspace::{AppState, WorkspaceStore};

fn init_app_state(cx: &mut App) -> anyhow::Result<Arc<AppState>> {
    // Set up filesystem and HTTP client
    let fs = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);

    // Use the reqwest client by default, then let `Client::production` wrap it with the configured URL.
    let http = Arc::new(reqwest_client::ReqwestClient::new());
    cx.set_http_client(http);

    // Client and language/runtime infrastructure
    let client = Client::production(cx);
    cx.set_http_client(client.http_client());

    let mut languages = LanguageRegistry::new(cx.background_executor().clone());
    languages.set_language_server_download_dir(paths::languages_dir().clone());
    let languages = Arc::new(languages);

    let (node_options_tx, node_options_rx) = watch::channel::<Option<NodeBinaryOptions>>(Some(
        NodeBinaryOptions {
            allow_path_lookup: true,
            allow_binary_download: false,
            use_paths: None,
        },
    ));
    // Keep sender alive so options stay available
    let _ = node_options_tx;
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);

    let session = cx
        .background_executor()
        .block(async move { Session::new(Uuid::new_v4().to_string()).await });
    let app_session = cx.new(|cx| AppSession::new(session, cx));
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));

    // Use a simple window builder for iOS (single window, no titlebar)
    let build_window_options = |_: Option<uuid::Uuid>, _cx: &mut App| WindowOptions {
        titlebar: None,
        focus: true,
        show: true,
        ..Default::default()
    };

    let app_state = Arc::new(AppState {
        languages,
        client: client.clone(),
        user_store,
        workspace_store,
        fs,
        build_window_options,
        node_runtime,
        session: app_session,
    });

    AppState::set_global(Arc::downgrade(&app_state), cx);
    Project::init(&client, cx);
    editor::init(cx);
    workspace::init(app_state.clone(), cx);
    project_panel::init(cx);

    Ok(app_state)
}

/// Main entry point for the iOS application.
/// Called from the Objective-C app delegate when the app finishes launching.
pub struct ZedIosApp;

impl ZedIosApp {
    /// Initialize the iOS application.
    /// This sets up the minimal required infrastructure and opens the main window.
    pub fn init(cx: &mut App) -> Result<()> {
        info!("ZedIosApp::init starting");

        // Initialize release channel (required by remote connection code)
        // Use Nightly channel to download the latest remote server binaries
        info!("Initializing release channel...");
        release_channel::init_test(
            semver::Version::new(0, 1, 0),
            release_channel::ReleaseChannel::Nightly,
            cx,
        );
        info!("Release channel initialized");

        // Initialize Tokio runtime for async networking (SSH, HTTP, etc.)
        info!("Initializing Tokio runtime...");
        gpui_tokio::init(cx);
        info!("Tokio runtime initialized");

        // Load embedded assets (fonts, icons, etc.)
        info!("Loading fonts...");

        // List what fonts we have
        if let Ok(font_paths) = cx.asset_source().list("fonts") {
            info!("Found {} font paths", font_paths.len());
            for path in font_paths.iter().take(5) {
                info!("  - {}", path);
            }
        }

        assets::Assets
            .load_fonts(cx)
            .map_err(|e| anyhow::anyhow!("Failed to load fonts: {}", e))?;
        info!("Fonts loaded");

        // List loaded font names
        let font_names = cx.text_system().all_font_names();
        info!("{} fonts available in text system", font_names.len());
        for name in font_names.iter().take(10) {
            info!("  - {}", name);
        }

        // Initialize settings with defaults
        info!("Initializing settings...");
        settings::init(cx);
        info!("Settings initialized");

        // Initialize theme system with base themes only
        info!("Initializing theme...");
        theme::init(theme::LoadThemes::JustBase, cx);
        info!("Theme initialized");

        // Initialize the core workspace stack for remote projects
        let app_state = init_app_state(cx)?;

        // Open the main window with root view
        info!("Opening window...");
        cx.open_window(
            WindowOptions {
                titlebar: None,
                focus: true,
                show: true,
                ..Default::default()
            },
            |window, cx| {
                info!("Creating RootView...");
                cx.new(|cx| RootView::new(app_state, window, cx))
            },
        )?;
        info!("Window opened successfully");

        Ok(())
    }
}

/// Entry point called from Objective-C app delegate.
/// This function is marked #[no_mangle] so it can be called by name from main.m
#[unsafe(no_mangle)]
pub extern "C" fn zed_ios_init() {
    info!("zed_ios_init called");

    // Initialize logging - default to debug level, output to stderr (Xcode console) and file
    zlog::try_init(Some("debug".to_string())).ok();
    zlog::init_output_stderr();

    // Also log to file for later retrieval
    std::fs::create_dir_all(logs_dir()).ok();
    if let Err(e) = zlog::init_output_file(log_file(), Some(old_log_file())) {
        eprintln!("Could not open log file: {}", e);
    }

    info!("Creating Application...");
    Application::new().with_assets(assets::Assets).run(|cx| {
        info!("Application::run callback executing");
        if let Err(e) = ZedIosApp::init(cx) {
            log::error!("Failed to initialize Zed iOS: {}", e);
        }
        info!("Application::run callback finished");
    });
    info!("Application::run returned");
}
