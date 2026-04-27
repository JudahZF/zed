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
mod process_mode;
mod remote_delegate;
mod root_view;
mod session_recovery;
mod text_input;
mod tutorial_view;
mod welcome_view;
mod workspace_view;

use agent_ui::AgentPanelDelegate;
use anyhow::Result;
use client::{Client, UserStore};
use db::kvp::{GLOBAL_KEY_VALUE_STORE, KEY_VALUE_STORE};
use fs::{Fs, RealFs};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Context, Entity, Pixels, ReadGlobal, Task,
    WeakEntity, Window, WindowOptions, px,
};
use language::LanguageRegistry;
use log::info;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use paths::{log_file, logs_dir, old_log_file};
use project::{DisableAiSettings, Project, trusted_worktrees};
use prompt_store::PromptBuilder;
use release_channel::ReleaseChannel;
use root_view::RootView;
use session::{AppSession, Session};
use session_recovery::SessionRecoveryCoordinator;
use settings::Settings;
use settings::SettingsStore;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;
use workspace::dock::{DockPosition, PanelHandle};
use workspace::{AppState, Panel, Workspace, WorkspaceStore};

use crate::mobile_feature_policy::MobileFeaturePolicy;
use crate::persistence::{ConnectionDb, MobileWorkspaceSnapshotV1};

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
    languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

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

pub(crate) fn load_mobile_workspace_snapshot(
    connection_profile_id: Option<i64>,
) -> Option<MobileWorkspaceSnapshotV1> {
    let connection_profile_id = connection_profile_id?;
    let state = ConnectionDb::open()
        .ok()?
        .load_session_restore_state()
        .ok()??;
    if state.connection_profile_id != connection_profile_id {
        return None;
    }

    state
        .workspace_state_json
        .as_deref()
        .and_then(|raw| MobileWorkspaceSnapshotV1::from_json(raw).ok())
}

fn mobile_ai_enabled(feature_policy: MobileFeaturePolicy, cx: &App) -> bool {
    feature_policy.show_ai
        && !SettingsStore::global(cx)
            .get::<DisableAiSettings>(None)
            .disable_ai
        && !cfg!(test)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobileLeftPanel {
    ProjectPanel,
    OutlinePanel,
}

impl MobileLeftPanel {
    fn persistent_name(self) -> &'static str {
        match self {
            Self::ProjectPanel => project_panel::ProjectPanel::persistent_name(),
            Self::OutlinePanel => outline_panel::OutlinePanel::persistent_name(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MobileRightPanel {
    GitPanel,
    AgentPanel,
}

impl MobileRightPanel {
    fn persistent_name(self) -> &'static str {
        match self {
            Self::GitPanel => git_ui::git_panel::GitPanel::persistent_name(),
            Self::AgentPanel => agent_ui::AgentPanel::persistent_name(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MobilePanelLayout {
    active_left_panel: Option<MobileLeftPanel>,
    active_right_panel: Option<MobileRightPanel>,
    show_terminal_panel: bool,
    left_dock_open: bool,
    right_dock_open: bool,
    bottom_dock_open: bool,
}

impl MobilePanelLayout {
    pub(crate) fn from_snapshot(snapshot: Option<&MobileWorkspaceSnapshotV1>) -> Self {
        let active_tool = snapshot.and_then(|snapshot| snapshot.active_tool.as_deref());

        let left_dock_open = snapshot
            .map(|snapshot| snapshot.left_sidebar_visible)
            .unwrap_or(true);
        let active_left_panel = left_dock_open.then_some(match active_tool {
            Some("outline") => MobileLeftPanel::OutlinePanel,
            _ => MobileLeftPanel::ProjectPanel,
        });

        let active_right_panel = if active_tool == Some("agent") {
            Some(MobileRightPanel::AgentPanel)
        } else if active_tool == Some("git") {
            Some(MobileRightPanel::GitPanel)
        } else if snapshot.is_some_and(|snapshot| snapshot.agent_visible) {
            Some(MobileRightPanel::AgentPanel)
        } else if snapshot.is_some_and(|snapshot| snapshot.git_panel_visible) {
            Some(MobileRightPanel::GitPanel)
        } else {
            None
        };
        let right_dock_open = active_right_panel.is_some();

        let show_terminal_panel = snapshot
            .is_some_and(|snapshot| snapshot.terminal_visible || active_tool == Some("terminal"));
        let bottom_dock_open = show_terminal_panel;

        Self {
            active_left_panel,
            active_right_panel,
            show_terminal_panel,
            left_dock_open,
            right_dock_open,
            bottom_dock_open,
        }
    }
}

fn apply_mobile_dock_layout(
    workspace: &mut Workspace,
    dock_position: DockPosition,
    active_panel_name: Option<&'static str>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace
        .dock_at_position(dock_position)
        .update(cx, |dock, cx| {
            match active_panel_name.and_then(|active_panel_name| {
                dock.panel_index_for_persistent_name(active_panel_name, cx)
            }) {
                Some(panel_index) => {
                    dock.activate_panel(panel_index, window, cx);
                    dock.set_open(true, window, cx);
                }
                None => {
                    dock.set_open(false, window, cx);
                }
            }
        });
}

pub(crate) fn apply_mobile_panel_layout(
    workspace: &mut Workspace,
    layout: &MobilePanelLayout,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    apply_mobile_dock_layout(
        workspace,
        DockPosition::Left,
        layout
            .left_dock_open
            .then_some(
                layout
                    .active_left_panel
                    .map(MobileLeftPanel::persistent_name),
            )
            .flatten(),
        window,
        cx,
    );
    apply_mobile_dock_layout(
        workspace,
        DockPosition::Right,
        layout
            .right_dock_open
            .then_some(
                layout
                    .active_right_panel
                    .map(MobileRightPanel::persistent_name),
            )
            .flatten(),
        window,
        cx,
    );
    apply_mobile_dock_layout(
        workspace,
        DockPosition::Bottom,
        layout
            .bottom_dock_open
            .then_some(terminal_view::terminal_panel::TerminalPanel::persistent_name()),
        window,
        cx,
    );

    let left_dock = workspace.left_dock().read(cx);
    let left_is_open = left_dock.is_open();
    let left_active_panel = left_dock
        .active_panel()
        .map(|panel| panel.panel_key())
        .unwrap_or("none");
    let right_dock = workspace.right_dock().read(cx);
    let right_is_open = right_dock.is_open();
    let right_active_panel = right_dock
        .active_panel()
        .map(|panel| panel.panel_key())
        .unwrap_or("none");
    let bottom_dock = workspace.bottom_dock().read(cx);
    let bottom_is_open = bottom_dock.is_open();
    let bottom_active_panel = bottom_dock
        .active_panel()
        .map(|panel| panel.panel_key())
        .unwrap_or("none");

    log::info!(
        "[Zed iOS] Reapplied mobile panel layout: requested={layout:?}, left(open={left_is_open}, active={left_active_panel}), right(open={right_is_open}, active={right_active_panel}), bottom(open={bottom_is_open}, active={bottom_active_panel})",
    );
}

#[cfg(any(test, target_os = "ios"))]
fn synthesized_ios_process_environment(
    existing_environment: &HashMap<String, String>,
    home_directory: &std::path::Path,
) -> std::collections::BTreeMap<String, String> {
    let mut synthesized_environment = std::collections::BTreeMap::new();

    if !existing_environment.contains_key("USER") {
        synthesized_environment.insert("USER".to_string(), "zed_ios".to_string());
    }
    if !existing_environment.contains_key("HOME") {
        synthesized_environment.insert(
            "HOME".to_string(),
            home_directory.to_string_lossy().into_owned(),
        );
    }
    if !existing_environment.contains_key("SHELL") {
        synthesized_environment.insert("SHELL".to_string(), "/bin/sh".to_string());
    }

    synthesized_environment
}

#[cfg(target_os = "ios")]
fn bootstrap_ios_process_environment() {
    let existing_environment = ["USER", "HOME", "SHELL"]
        .into_iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| (key.to_string(), value))
        })
        .collect::<HashMap<_, _>>();
    let synthesized_environment =
        synthesized_ios_process_environment(&existing_environment, paths::home_dir().as_path());

    if synthesized_environment.is_empty() {
        return;
    }

    for (key, value) in &synthesized_environment {
        unsafe {
            std::env::set_var(key, value);
        }
    }

    let synthesized_keys = synthesized_environment.keys().cloned().collect::<Vec<_>>();
    log::info!(
        "[Zed iOS] Bootstrapped iOS process environment keys: {}",
        synthesized_keys.join(", ")
    );
}

#[cfg(not(target_os = "ios"))]
fn bootstrap_ios_process_environment() {}

fn attach_mobile_panel<P: Panel>(
    workspace_handle: &WeakEntity<Workspace>,
    panel: Entity<P>,
    size: Option<Pixels>,
    open_after_attach: bool,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<()> {
    workspace_handle.update_in(&mut cx, |workspace, window, cx| {
        if let Some(existing_panel) = workspace.panel::<P>(cx) {
            log::info!(
                "[Zed iOS] Skipping duplicate attach for panel `{}`",
                P::panel_key()
            );
            if let Some(size) = size {
                existing_panel.set_size(Some(size), window, cx);
            }
            if open_after_attach {
                workspace.open_panel::<P>(window, cx);
            }
            return;
        }

        panel.set_size(size, window, cx);
        workspace.add_panel(panel, window, cx);
        if open_after_attach {
            workspace.open_panel::<P>(window, cx);
        }
    })?;

    Ok(())
}

async fn initialize_mobile_agent_panel(
    workspace_handle: WeakEntity<Workspace>,
    prompt_builder: Arc<PromptBuilder>,
    feature_policy: MobileFeaturePolicy,
    restored_snapshot: Option<MobileWorkspaceSnapshotV1>,
    mut cx: AsyncWindowContext,
) -> anyhow::Result<()> {
    let should_open_agent = restored_snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.agent_visible || snapshot.active_tool.as_deref() == Some("agent")
    });
    let should_enable_agent = cx
        .update(|_, cx| mobile_ai_enabled(feature_policy, cx))
        .unwrap_or(false);
    if !should_enable_agent {
        return Ok(());
    }

    let panel =
        agent_ui::AgentPanel::load(workspace_handle.clone(), prompt_builder, cx.clone()).await?;
    attach_mobile_panel(&workspace_handle, panel, None, should_open_agent, cx)
}

pub(crate) fn initialize_mobile_panels(
    prompt_builder: Arc<PromptBuilder>,
    feature_policy: MobileFeaturePolicy,
    restored_snapshot: Option<MobileWorkspaceSnapshotV1>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Task<anyhow::Result<()>> {
    cx.spawn_in(window, async move |workspace_handle, cx| {
        let mobile_panel_layout = MobilePanelLayout::from_snapshot(restored_snapshot.as_ref());
        let sidebar_width = if feature_policy.compact_panels {
            px(224.0)
        } else {
            px(256.0)
        };
        let git_panel_width = if feature_policy.mobile_git_modals {
            if feature_policy.compact_panels {
                px(280.0)
            } else {
                px(308.0)
            }
        } else if feature_policy.compact_panels {
            px(300.0)
        } else {
            px(340.0)
        };
        let terminal_height = feature_policy.terminal_default_height;
        let open_project_panel = mobile_panel_layout.left_dock_open
            && mobile_panel_layout.active_left_panel == Some(MobileLeftPanel::ProjectPanel);
        let open_outline_panel = mobile_panel_layout.left_dock_open
            && mobile_panel_layout.active_left_panel == Some(MobileLeftPanel::OutlinePanel);
        let open_git_panel = mobile_panel_layout.right_dock_open
            && mobile_panel_layout.active_right_panel == Some(MobileRightPanel::GitPanel);
        let open_terminal_panel = mobile_panel_layout.show_terminal_panel;

        match project_panel::ProjectPanel::load(workspace_handle.clone(), cx.clone()).await {
            Ok(panel) => {
                if let Err(err) = attach_mobile_panel(
                    &workspace_handle,
                    panel,
                    Some(sidebar_width),
                    open_project_panel,
                    cx.clone(),
                ) {
                    log::error!("[Zed iOS] Failed to attach project panel: {err:#}");
                }
            }
            Err(err) => log::error!("[Zed iOS] Failed to load project panel: {err:#}"),
        }

        match outline_panel::OutlinePanel::load(workspace_handle.clone(), cx.clone()).await {
            Ok(panel) => {
                if let Err(err) = attach_mobile_panel(
                    &workspace_handle,
                    panel,
                    Some(sidebar_width),
                    open_outline_panel,
                    cx.clone(),
                ) {
                    log::error!("[Zed iOS] Failed to attach outline panel: {err:#}");
                }
            }
            Err(err) => log::error!("[Zed iOS] Failed to load outline panel: {err:#}"),
        }

        match git_ui::git_panel::GitPanel::load(workspace_handle.clone(), cx.clone()).await {
            Ok(panel) => {
                if let Err(err) = attach_mobile_panel(
                    &workspace_handle,
                    panel,
                    Some(git_panel_width),
                    open_git_panel,
                    cx.clone(),
                ) {
                    log::error!("[Zed iOS] Failed to attach git panel: {err:#}");
                }
            }
            Err(err) => log::error!("[Zed iOS] Failed to load git panel: {err:#}"),
        }

        match terminal_view::terminal_panel::TerminalPanel::load(
            workspace_handle.clone(),
            cx.clone(),
        )
        .await
        {
            Ok(panel) => {
                if let Err(err) = attach_mobile_panel(
                    &workspace_handle,
                    panel,
                    Some(terminal_height),
                    open_terminal_panel,
                    cx.clone(),
                ) {
                    log::error!("[Zed iOS] Failed to attach terminal panel: {err:#}");
                }
            }
            Err(err) => log::error!("[Zed iOS] Failed to load terminal panel: {err:#}"),
        }

        if let Err(err) = initialize_mobile_agent_panel(
            workspace_handle.clone(),
            prompt_builder,
            feature_policy,
            restored_snapshot.clone(),
            cx.clone(),
        )
        .await
        {
            log::error!("[Zed iOS] Failed to initialize agent panel: {err:#}");
        }

        anyhow::Ok(())
    })
}

pub(crate) fn register_mobile_actions(
    feature_policy: MobileFeaturePolicy,
    workspace: &mut Workspace,
    cx: &mut Context<Workspace>,
) {
    if !mobile_ai_enabled(feature_policy, cx) {
        return;
    }

    <dyn AgentPanelDelegate>::set_global(Arc::new(agent_ui::ConcreteAssistantPanelDelegate), cx);
    workspace
        .register_action(agent_ui::AgentPanel::toggle_focus)
        .register_action(agent_ui::AgentPanel::toggle)
        .register_action(agent_ui::InlineAssistant::inline_assist);
}

/// Main entry point for the iOS application.
/// Called from the Objective-C app delegate when the app finishes launching.
pub struct ZedIosApp;

impl ZedIosApp {
    /// Initialize the iOS application.
    /// This sets up the minimal required infrastructure and opens the main window.
    pub fn init(cx: &mut App) -> Result<()> {
        info!("ZedIosApp::init starting");
        bootstrap_ios_process_environment();

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
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(|cx| {
            info!("Application::run callback executing");
            if let Err(e) = ZedIosApp::init(cx) {
                log::error!("Failed to initialize Zed iOS: {}", e);
            }
            info!("Application::run callback finished");
        });
    info!("Application::run returned");
}

#[unsafe(no_mangle)]
pub extern "C" fn zed_ios_maybe_run_process_mode(
    argc: i32,
    argv: *const *const std::ffi::c_char,
) -> i32 {
    process_mode::maybe_run_process_mode(argc, argv)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;
    use std::sync::Arc;

    use fs::FakeFs;
    use gpui::{
        Action, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable,
        InteractiveElement, IntoElement, Pixels, Render, TestAppContext, Window, actions, div, px,
    };
    use node_runtime::NodeRuntime;
    use project::Project;
    use session::{AppSession, Session};
    use settings::SettingsStore;
    use workspace::dock::{DockPosition, PanelEvent};
    use workspace::{AppState, Panel, Workspace, WorkspaceStore};

    use super::{
        MobilePanelLayout, apply_mobile_panel_layout, attach_mobile_panel,
        synthesized_ios_process_environment,
    };
    use crate::MobileWorkspaceSnapshotV1;

    actions!(
        zed_ios_tests,
        [
            ToggleTestMobilePanel,
            ToggleTestFilesMobilePanel,
            ToggleTestOutlineMobilePanel,
            ToggleTestGitMobilePanel,
            ToggleTestTerminalMobilePanel
        ]
    );

    struct TestMobilePanel {
        focus_handle: FocusHandle,
        size: Pixels,
        is_active: bool,
        is_zoomed: bool,
    }

    impl TestMobilePanel {
        fn new(cx: &mut App) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                size: px(300.0),
                is_active: false,
                is_zoomed: false,
            }
        }
    }

    impl EventEmitter<PanelEvent> for TestMobilePanel {}

    impl Focusable for TestMobilePanel {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for TestMobilePanel {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().track_focus(&self.focus_handle(cx))
        }
    }

    impl Panel for TestMobilePanel {
        fn persistent_name() -> &'static str {
            "TestMobilePanel"
        }

        fn panel_key() -> &'static str {
            "TestMobilePanel"
        }

        fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
            DockPosition::Right
        }

        fn position_is_valid(&self, position: DockPosition) -> bool {
            position == DockPosition::Right
        }

        fn set_position(
            &mut self,
            _position: DockPosition,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
        }

        fn size(&self, _window: &Window, _cx: &App) -> Pixels {
            self.size
        }

        fn set_size(
            &mut self,
            size: Option<Pixels>,
            _window: &mut Window,
            _cx: &mut Context<Self>,
        ) {
            self.size = size.unwrap_or(px(300.0));
        }

        fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
            None
        }

        fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
            None
        }

        fn toggle_action(&self) -> Box<dyn Action> {
            ToggleTestMobilePanel.boxed_clone()
        }

        fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
            self.is_zoomed
        }

        fn set_zoomed(&mut self, zoomed: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.is_zoomed = zoomed;
        }

        fn set_active(&mut self, is_active: bool, _window: &mut Window, _cx: &mut Context<Self>) {
            self.is_active = is_active;
        }

        fn activation_priority(&self) -> u32 {
            100
        }
    }

    macro_rules! define_positioned_test_mobile_panel {
        ($panel_name:ident, $toggle_action:ident, $panel_type:path, $dock_position:expr, $activation_priority:expr) => {
            struct $panel_name {
                focus_handle: FocusHandle,
                size: Pixels,
                is_active: bool,
                is_zoomed: bool,
            }

            impl $panel_name {
                fn new(cx: &mut App) -> Self {
                    Self {
                        focus_handle: cx.focus_handle(),
                        size: px(300.0),
                        is_active: false,
                        is_zoomed: false,
                    }
                }
            }

            impl EventEmitter<PanelEvent> for $panel_name {}

            impl Focusable for $panel_name {
                fn focus_handle(&self, _cx: &App) -> FocusHandle {
                    self.focus_handle.clone()
                }
            }

            impl Render for $panel_name {
                fn render(
                    &mut self,
                    _window: &mut Window,
                    cx: &mut Context<Self>,
                ) -> impl IntoElement {
                    div().track_focus(&self.focus_handle(cx))
                }
            }

            impl Panel for $panel_name {
                fn persistent_name() -> &'static str {
                    <$panel_type as Panel>::persistent_name()
                }

                fn panel_key() -> &'static str {
                    <$panel_type as Panel>::panel_key()
                }

                fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
                    $dock_position
                }

                fn position_is_valid(&self, position: DockPosition) -> bool {
                    position == $dock_position
                }

                fn set_position(
                    &mut self,
                    _position: DockPosition,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                }

                fn size(&self, _window: &Window, _cx: &App) -> Pixels {
                    self.size
                }

                fn set_size(
                    &mut self,
                    size: Option<Pixels>,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                    self.size = size.unwrap_or(px(300.0));
                }

                fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
                    None
                }

                fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
                    None
                }

                fn toggle_action(&self) -> Box<dyn Action> {
                    $toggle_action.boxed_clone()
                }

                fn is_zoomed(&self, _window: &Window, _cx: &App) -> bool {
                    self.is_zoomed
                }

                fn set_zoomed(
                    &mut self,
                    zoomed: bool,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                    self.is_zoomed = zoomed;
                }

                fn set_active(
                    &mut self,
                    is_active: bool,
                    _window: &mut Window,
                    _cx: &mut Context<Self>,
                ) {
                    self.is_active = is_active;
                }

                fn activation_priority(&self) -> u32 {
                    $activation_priority
                }
            }
        };
    }

    define_positioned_test_mobile_panel!(
        TestFilesMobilePanel,
        ToggleTestFilesMobilePanel,
        project_panel::ProjectPanel,
        DockPosition::Left,
        100
    );
    define_positioned_test_mobile_panel!(
        TestOutlineMobilePanel,
        ToggleTestOutlineMobilePanel,
        outline_panel::OutlinePanel,
        DockPosition::Left,
        110
    );
    define_positioned_test_mobile_panel!(
        TestGitMobilePanel,
        ToggleTestGitMobilePanel,
        git_ui::git_panel::GitPanel,
        DockPosition::Right,
        120
    );
    define_positioned_test_mobile_panel!(
        TestTerminalMobilePanel,
        ToggleTestTerminalMobilePanel,
        terminal_view::terminal_panel::TerminalPanel,
        DockPosition::Bottom,
        130
    );

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[gpui::test]
    async fn attach_mobile_panel_is_idempotent_for_duplicate_panel_types(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (workspace, cx) = cx.add_window_view({
            let project = project.clone();
            move |window, cx| {
                let client = project.read(cx).client();
                let user_store = project.read(cx).user_store();
                let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
                let session = cx.new(|cx| AppSession::new(Session::test(), cx));
                let app_state = Arc::new(AppState {
                    languages: project.read(cx).languages().clone(),
                    client,
                    user_store,
                    workspace_store,
                    fs: project.read(cx).fs().clone(),
                    build_window_options: |_, _| Default::default(),
                    node_runtime: NodeRuntime::unavailable(),
                    session,
                });

                AppState::set_global(Arc::downgrade(&app_state), cx);
                window.activate_window();

                Workspace::new(None, project.clone(), app_state, window, cx)
            }
        });

        let first_panel = cx.new(|cx| TestMobilePanel::new(cx));
        let duplicate_panel = cx.new(|cx| TestMobilePanel::new(cx));
        let first_panel_id = first_panel.entity_id();
        let duplicate_panel_id = duplicate_panel.entity_id();

        let first_window_cx = cx.update(|window, cx| window.to_async(cx));
        attach_mobile_panel(
            &workspace.downgrade(),
            first_panel.clone(),
            Some(px(240.0)),
            false,
            first_window_cx,
        )
        .unwrap();

        let second_window_cx = cx.update(|window, cx| window.to_async(cx));
        attach_mobile_panel(
            &workspace.downgrade(),
            duplicate_panel,
            Some(px(320.0)),
            true,
            second_window_cx,
        )
        .unwrap();

        workspace.read_with(cx, |workspace, cx| {
            let installed_panel = workspace
                .panel::<TestMobilePanel>(cx)
                .expect("test panel should be attached");

            assert_eq!(workspace.right_dock().read(cx).panels_len(), 1);
            assert_eq!(installed_panel.entity_id(), first_panel_id);
            assert_ne!(duplicate_panel_id, first_panel_id);
            assert_eq!(installed_panel.read(cx).size, px(320.0));
            assert!(workspace.right_dock().read(cx).is_open());
        });
    }

    #[gpui::test]
    async fn apply_mobile_panel_layout_reopens_snapshot_requested_docks(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (workspace, cx) = cx.add_window_view({
            let project = project.clone();
            move |window, cx| {
                let client = project.read(cx).client();
                let user_store = project.read(cx).user_store();
                let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));
                let session = cx.new(|cx| AppSession::new(Session::test(), cx));
                let app_state = Arc::new(AppState {
                    languages: project.read(cx).languages().clone(),
                    client,
                    user_store,
                    workspace_store,
                    fs: project.read(cx).fs().clone(),
                    build_window_options: |_, _| Default::default(),
                    node_runtime: NodeRuntime::unavailable(),
                    session,
                });

                AppState::set_global(Arc::downgrade(&app_state), cx);
                window.activate_window();

                Workspace::new(None, project.clone(), app_state, window, cx)
            }
        });

        let files_panel = cx.new(|cx| TestFilesMobilePanel::new(cx));
        let outline_panel = cx.new(|cx| TestOutlineMobilePanel::new(cx));
        let git_panel = cx.new(|cx| TestGitMobilePanel::new(cx));
        let terminal_panel = cx.new(|cx| TestTerminalMobilePanel::new(cx));

        attach_mobile_panel(
            &workspace.downgrade(),
            files_panel,
            Some(px(240.0)),
            true,
            cx.update(|window, cx| window.to_async(cx)),
        )
        .unwrap();
        attach_mobile_panel(
            &workspace.downgrade(),
            outline_panel,
            Some(px(240.0)),
            false,
            cx.update(|window, cx| window.to_async(cx)),
        )
        .unwrap();
        attach_mobile_panel(
            &workspace.downgrade(),
            git_panel,
            Some(px(280.0)),
            false,
            cx.update(|window, cx| window.to_async(cx)),
        )
        .unwrap();
        attach_mobile_panel(
            &workspace.downgrade(),
            terminal_panel,
            Some(px(200.0)),
            true,
            cx.update(|window, cx| window.to_async(cx)),
        )
        .unwrap();

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace
                    .left_dock()
                    .update(cx, |dock, cx| dock.set_open(false, window, cx));
            });
        });

        let snapshot = MobileWorkspaceSnapshotV1 {
            active_tool: Some("files".to_string()),
            left_sidebar_visible: true,
            terminal_visible: true,
            ..MobileWorkspaceSnapshotV1::new(Some("~/zed".to_string()), Some("~/zed".to_string()))
        };
        let layout = MobilePanelLayout::from_snapshot(Some(&snapshot));

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                apply_mobile_panel_layout(workspace, &layout, window, cx);
            });
        });

        workspace.read_with(cx, |workspace, cx| {
            let left_dock = workspace.left_dock().read(cx);
            assert!(left_dock.is_open());
            assert_eq!(
                left_dock
                    .active_panel()
                    .map(|panel| panel.persistent_name()),
                Some(project_panel::ProjectPanel::persistent_name())
            );

            let bottom_dock = workspace.bottom_dock().read(cx);
            assert!(bottom_dock.is_open());
            assert_eq!(
                bottom_dock
                    .active_panel()
                    .map(|panel| panel.persistent_name()),
                Some(terminal_view::terminal_panel::TerminalPanel::persistent_name())
            );
        });
    }

    #[test]
    fn synthesized_ios_process_environment_only_fills_missing_values() {
        let existing_environment = HashMap::from([
            ("HOME".to_string(), "/existing/home".to_string()),
            ("USER".to_string(), "existing_user".to_string()),
        ]);

        let synthesized_environment =
            synthesized_ios_process_environment(&existing_environment, Path::new("/Users/zed_ios"));

        assert_eq!(
            synthesized_environment,
            BTreeMap::from([("SHELL".to_string(), "/bin/sh".to_string())])
        );

        let mut merged_environment = existing_environment.clone();
        for (key, value) in synthesized_environment {
            merged_environment.entry(key).or_insert(value);
        }

        assert_eq!(
            merged_environment.get("USER"),
            Some(&"existing_user".to_string())
        );
        assert_eq!(
            merged_environment.get("HOME"),
            Some(&"/existing/home".to_string())
        );
        assert_eq!(
            merged_environment.get("SHELL"),
            Some(&"/bin/sh".to_string())
        );

        let synthesized_when_missing =
            synthesized_ios_process_environment(&HashMap::default(), Path::new("/Users/zed_ios"));
        assert_eq!(
            synthesized_when_missing,
            BTreeMap::from([
                ("HOME".to_string(), "/Users/zed_ios".to_string()),
                ("SHELL".to_string(), "/bin/sh".to_string()),
                ("USER".to_string(), "zed_ios".to_string()),
            ])
        );
    }
}
