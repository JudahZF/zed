//! Zed for iPadOS - Remote-first code editor
//!
//! This crate provides the iOS application entry point for Zed,
//! a thin client that connects to remote development machines.

mod connect_view;
mod mobile_feature_policy;
mod mobile_settings;
mod mobile_workspace_chrome;
mod persistence;
mod port_forward_manager;
mod remote_delegate;
mod root_view;
mod session_recovery;
mod text_input;
mod tutorial_view;
mod welcome_view;
mod workspace_view;

use anyhow::Result;
use client::{Client, UserStore};
use db::kvp::{GLOBAL_KEY_VALUE_STORE, KEY_VALUE_STORE};
use fs::{Fs, RealFs};
use gpui::{App, AppContext as _, WindowOptions};
use language::LanguageRegistry;
use log::info;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use paths::{log_file, logs_dir, old_log_file};
use project::{Project, trusted_worktrees};
use prompt_store::PromptBuilder;
use release_channel::ReleaseChannel;
use root_view::RootView;
use session::{AppSession, Session};
use session_recovery::SessionRecoveryCoordinator;
use settings::Settings;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;
use workspace::{AppState, WorkspaceStore};

const ZED_DESKTOP_CARGO_TOML: &str = include_str!("../../zed/Cargo.toml");

pub(crate) fn ios_app_version_string() -> &'static str {
    for line in ZED_DESKTOP_CARGO_TOML.lines() {
        let trimmed = line.trim();
        if let Some(version) = trimmed.strip_prefix("version = \"") {
            if let Some(version) = version.strip_suffix('"') {
                return version;
            }
        }
    }

    panic!("failed to determine Zed app version from crates/zed/Cargo.toml");
}

pub(crate) fn ios_app_version() -> semver::Version {
    semver::Version::parse(ios_app_version_string())
        .expect("desktop zed package version must be valid semver")
}

pub(crate) fn ios_release_channel() -> ReleaseChannel {
    ReleaseChannel::Preview
}

#[derive(Clone, Debug)]
enum IdType {
    New(String),
    Existing(String),
}

impl std::fmt::Display for IdType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::New(id) | Self::Existing(id) => f.write_str(id),
        }
    }
}

async fn system_id() -> anyhow::Result<IdType> {
    let key_name = "system_id".to_string();

    if let Ok(Some(system_id)) = GLOBAL_KEY_VALUE_STORE.read_kvp(&key_name) {
        return Ok(IdType::Existing(system_id));
    }

    let system_id = Uuid::new_v4().to_string();
    GLOBAL_KEY_VALUE_STORE
        .write_kvp(key_name, system_id.clone())
        .await?;

    Ok(IdType::New(system_id))
}

async fn installation_id() -> anyhow::Result<IdType> {
    let legacy_key_name = "device_id".to_string();
    let key_name = "installation_id".to_string();

    if let Ok(Some(installation_id)) = KEY_VALUE_STORE.read_kvp(&legacy_key_name) {
        KEY_VALUE_STORE
            .write_kvp(key_name, installation_id.clone())
            .await?;
        KEY_VALUE_STORE.delete_kvp(legacy_key_name).await?;
        return Ok(IdType::Existing(installation_id));
    }

    if let Ok(Some(installation_id)) = KEY_VALUE_STORE.read_kvp(&key_name) {
        return Ok(IdType::Existing(installation_id));
    }

    let installation_id = Uuid::new_v4().to_string();
    KEY_VALUE_STORE
        .write_kvp(key_name, installation_id.clone())
        .await?;

    Ok(IdType::New(installation_id))
}

fn init_ios_telemetry(client: &Arc<Client>, session: &Session, cx: &mut App) {
    let system_id = match cx.foreground_executor().block_on(system_id()) {
        Ok(system_id) => Some(system_id),
        Err(err) => {
            log::warn!("Failed to load iOS telemetry system_id: {err:#}");
            None
        }
    };
    let installation_id = match cx.foreground_executor().block_on(installation_id()) {
        Ok(installation_id) => Some(installation_id),
        Err(err) => {
            log::warn!("Failed to load iOS telemetry installation_id: {err:#}");
            None
        }
    };

    let telemetry = client.telemetry();
    telemetry.start(
        system_id.as_ref().map(ToString::to_string),
        installation_id.as_ref().map(ToString::to_string),
        session.id().to_owned(),
        cx,
    );

    if let (Some(system_id), Some(installation_id)) = (&system_id, &installation_id) {
        match (system_id, installation_id) {
            (IdType::New(_), IdType::New(_)) => {
                telemetry::event!("App First Opened");
                telemetry::event!("App First Opened For Release Channel");
            }
            (IdType::Existing(_), IdType::New(_)) => {
                telemetry::event!("App First Opened For Release Channel");
            }
            _ => {
                telemetry::event!("App Opened");
            }
        }
    } else {
        telemetry::event!("App Opened");
    }
}

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

    let (node_options_tx, node_options_rx) =
        watch::channel::<Option<NodeBinaryOptions>>(Some(NodeBinaryOptions {
            allow_path_lookup: true,
            allow_binary_download: false,
            use_paths: None,
        }));
    // Keep sender alive so options stay available
    let _ = node_options_tx;
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);

    let session = cx
        .foreground_executor()
        .block_on(async move { Session::new(Uuid::new_v4().to_string()).await });
    init_ios_telemetry(&client, &session, cx);
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

    // Keep trusted-worktree behavior consistent with desktop.
    let db_trusted_paths = match workspace::WORKSPACE_DB.fetch_trusted_worktrees() {
        Ok(trusted_paths) => trusted_paths,
        Err(err) => {
            log::error!("Failed to load trusted worktrees on iOS startup: {err:#}");
            HashMap::default()
        }
    };
    trusted_worktrees::init(db_trusted_paths, cx);

    AppState::set_global(Arc::downgrade(&app_state), cx);
    Project::init(&client, cx);

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
        info!("Initializing release channel...");
        let app_version = ios_app_version();
        let release_channel = ios_release_channel();
        release_channel::init_test(app_version.clone(), release_channel, cx);
        info!(
            "Release channel initialized: {:?} {}",
            release_channel, app_version
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
        mobile_settings::MobileSettings::register(cx);
        info!("Settings initialized");

        // Initialize theme system with base themes only
        info!("Initializing theme...");
        theme::init(theme::LoadThemes::JustBase, cx);
        info!("Theme initialized");

        // Initialize the core workspace stack for remote projects
        let app_state = init_app_state(cx)?;
        language_model::init(app_state.user_store.clone(), app_state.client.clone(), cx);
        language_models::init(app_state.user_store.clone(), app_state.client.clone(), cx);
        prompt_store::init(cx);
        acp_tools::init(cx);
        git_ui::init(cx);
        let prompt_builder = PromptBuilder::load(app_state.fs.clone(), false, cx);
        agent_ui::init(
            app_state.fs.clone(),
            app_state.client.clone(),
            prompt_builder,
            app_state.languages.clone(),
            false,
            cx,
        );
        command_palette::init(cx);
        editor::init(cx);
        diagnostics::init(cx);
        workspace::init(app_state.clone(), cx);
        file_finder::init(cx);
        tab_switcher::init(cx);
        outline::init(cx);
        project_symbols::init(cx);
        project_panel::init(cx);
        outline_panel::init(cx);
        tasks_ui::init(cx);
        snippets_ui::init(cx);
        search::init(cx);
        terminal_view::init(cx);
        language_tools::init(cx);
        SessionRecoveryCoordinator::init(cx);
        let _ = cx.on_ios_lifecycle(|event, cx| {
            SessionRecoveryCoordinator::handle_ios_lifecycle_event(event, cx);
        });

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

    // Initialize logging from env (ZED_LOG/RUST_LOG) and avoid forcing debug output in release.
    zlog::try_init(None).ok();
    #[cfg(debug_assertions)]
    zlog::init_output_stderr();

    // Also log to file for later retrieval
    std::fs::create_dir_all(logs_dir()).ok();
    if let Err(e) = zlog::init_output_file(log_file(), Some(old_log_file())) {
        eprintln!("Could not open log file: {}", e);
    }

    info!("Creating Application...");
    gpui_platform::application().with_assets(assets::Assets).run(|cx| {
        info!("Application::run callback executing");
        if let Err(e) = ZedIosApp::init(cx) {
            log::error!("Failed to initialize Zed iOS: {}", e);
        }
        info!("Application::run callback finished");
    });
    info!("Application::run returned");
}
