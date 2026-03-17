//! Workspace view shown after successful SSH connection.
//!
//! This view shows loading/error states while setting up the remote workspace.
//! Once ready, it hands the restored shared workspace entity to the mobile
//! chrome so iPad can wrap the normal workspace stack instead of recreating it.

use gpui::{
    AnyWindowHandle, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    Render, Window, div, prelude::*, px, rgb,
};
use project::Project;
use remote::{RemoteClient, RemoteConnectionOptions, SshConnectionOptions};
use std::path::PathBuf;
use workspace::{AppState, Workspace};

use crate::mobile_feature_policy::MobileFeaturePolicy;
use crate::persistence::{ConnectionDb, SessionRestoreState, current_timestamp};

/// Event emitted when user wants to disconnect
pub struct DisconnectRequested;

/// Event emitted when workspace is ready and should replace the window root
pub struct WorkspaceReady {
    pub workspace: Entity<Workspace>,
    pub client: Entity<RemoteClient>,
    pub connection_profile_id: Option<i64>,
    pub remote_path: Option<String>,
}

/// Current state of the workspace loading process
#[derive(Clone)]
enum WorkspaceLoadState {
    /// Loading with a status message
    Loading(String),
    /// Workspace is ready
    Ready,
    /// Loading failed with an error
    Error(String),
    /// Attempting to reconnect after an error
    Reconnecting(String),
}

/// Workspace view for remote editing - handles loading states before
/// transitioning to the full workspace::Workspace
pub struct WorkspaceView {
    client: Entity<RemoteClient>,
    focus_handle: FocusHandle,
    connection_details: ConnectionDetails,
    load_state: WorkspaceLoadState,
    workspace: Option<Entity<Workspace>>,
    initial_path: Option<String>,
    connection_profile_id: Option<i64>,
}

impl WorkspaceView {
    pub fn new(
        client: Entity<RemoteClient>,
        initial_path: Option<String>,
        connection_profile_id: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connection_options = client.read(cx).connection_options();
        let connection_details = ConnectionDetails::from_options(&connection_options);

        let mut this = Self {
            client,
            focus_handle: cx.focus_handle(),
            connection_details,
            load_state: WorkspaceLoadState::Loading("Initializing...".to_string()),
            workspace: None,
            initial_path,
            connection_profile_id,
        };

        this.start_workspace_load(window, cx);
        this
    }

    fn disconnect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Disconnect requested");
        self.workspace = None;
        cx.emit(DisconnectRequested);
    }

    fn retry_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Retrying connection...");
        self.load_state = WorkspaceLoadState::Reconnecting("Reconnecting...".to_string());
        self.workspace = None;
        cx.notify();

        self.start_workspace_load(window, cx);
    }

    fn start_workspace_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = match AppState::try_global(cx).and_then(|state| state.upgrade()) {
            Some(state) => state,
            None => {
                self.load_state =
                    WorkspaceLoadState::Error("Workspace stack not initialized".to_string());
                cx.notify();
                return;
            }
        };

        let client = self.client.clone();
        let connection_options = client.read(cx).connection_options();
        let use_shared_remote_restore = MobileFeaturePolicy::from_app(cx).use_shared_remote_restore;
        self.load_state = WorkspaceLoadState::Loading("Creating remote project...".to_string());
        cx.notify();

        // Create the project entity for the remote connection
        let project = Project::remote(
            client,
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            true,
            cx,
        );
        let workspace =
            cx.new(|cx| Workspace::new(None, project.clone(), app_state.clone(), window, cx));
        let window_handle: AnyWindowHandle = window.window_handle();
        self.workspace = Some(workspace.clone());

        self.load_state = WorkspaceLoadState::Loading("Restoring remote workspace...".to_string());
        cx.notify();

        // Open the home directory on the remote
        let path = self
            .initial_path
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "~".to_string());

        let paths = [PathBuf::from(path)];

        cx.spawn(async move |this, cx| {
            // Ensure the requested worktrees exist before asking the shared workspace
            // layer to restore its serialized state for these remote roots.
            let add_tasks = match cx.update(|cx| {
                project.update(cx, |project, cx| {
                    paths
                        .iter()
                        .map(|path| project.find_or_create_worktree(path, true, cx))
                        .collect::<Vec<_>>()
                })
            }) {
                Ok(tasks) => tasks,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        log::error!("[Zed iOS] Failed to queue remote worktree creation: {err:#}");
                        this.load_state =
                            WorkspaceLoadState::Error(format!("Failed to open project: {err:#}"));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let mut canonical_paths = Vec::with_capacity(add_tasks.len());
            for task in add_tasks {
                let (worktree, relative_path) = match task.await {
                    Ok(result) => result,
                    Err(err) => {
                        this.update(cx, |this, cx| {
                            log::error!("[Zed iOS] Failed to create remote worktree: {err:#}");
                            this.load_state =
                                WorkspaceLoadState::Error(format!("Failed to open project: {err:#}"));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };

                let canonical_path = match worktree.read_with(cx, |worktree, _| {
                    if relative_path.is_empty() {
                        worktree.abs_path().as_ref().to_path_buf()
                    } else {
                        worktree.absolutize(&relative_path)
                    }
                }) {
                    Ok(path) => path,
                    Err(err) => {
                        this.update(cx, |this, cx| {
                            log::error!(
                                "[Zed iOS] Failed to resolve canonical remote path: {err:#}"
                            );
                            this.load_state =
                                WorkspaceLoadState::Error(format!("Failed to open project: {err:#}"));
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                };

                canonical_paths.push(canonical_path);
            }

            let opened_remote_path = canonical_paths
                .first()
                .map(|path| path.to_string_lossy().into_owned());

            let restore_result = {
                let paths = canonical_paths;
                if use_shared_remote_restore {
                    match window_handle.update(cx, move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.restore_remote_project(
                                connection_options.clone(),
                                paths,
                                window,
                                cx,
                            )
                        })
                    }) {
                        Ok(task) => task.await.map(|_| ()),
                        Err(err) => Err(err),
                    }
                } else {
                    match window_handle.update(cx, move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.open_paths(
                                paths,
                                workspace::OpenOptions::default(),
                                None,
                                window,
                                cx,
                            )
                        })
                    }) {
                        Ok(task) => {
                            let _ = task.await;
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
            };

            this.update(cx, |this, cx| {
                match restore_result {
                    Ok(_) => {
                        log::info!("[Zed iOS] Remote project opened successfully");
                        let remote_path = opened_remote_path.clone().or_else(|| {
                            this.initial_path
                                .clone()
                                .filter(|path| !path.trim().is_empty())
                        });

                        if let Some(connection_profile_id) = this.connection_profile_id {
                            if let Some(remote_path) = remote_path.clone() {
                                if let Err(err) = ConnectionDb::open().and_then(|db| {
                                    db.save_session_restore_state(&SessionRestoreState {
                                        connection_profile_id,
                                        remote_path: Some(remote_path.clone()),
                                        last_opened_worktree: Some(remote_path),
                                        workspace_state_json: None,
                                        updated_at: current_timestamp(),
                                    })
                                }) {
                                    log::warn!(
                                        "[Zed iOS] Failed to save session restore state after workspace open: {err}"
                                    );
                                }
                            }

                        }
                        this.load_state = WorkspaceLoadState::Ready;

                        // Emit event to trigger workspace installation
                        if let Some(workspace) = this.workspace.clone() {
                            cx.emit(WorkspaceReady {
                                workspace,
                                client: this.client.clone(),
                                connection_profile_id: this.connection_profile_id,
                                remote_path,
                            });
                        }
                    }
                    Err(err) => {
                        log::error!("[Zed iOS] Failed to open remote project: {err:#}");
                        this.load_state =
                            WorkspaceLoadState::Error(format!("Failed to open project: {err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Renders a compact status bar at the top
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let surface_color = rgb(0x181825);
        let text_color = rgb(0xcdd6f4);
        let muted_color = rgb(0xa6adc8);
        let accent_color = rgb(0xa6e3a1);
        let warning_color = rgb(0xf9e2af);
        let error_color = rgb(0xf38ba8);
        let button_bg = rgb(0x313244);
        let button_hover = rgb(0x45475a);

        let (status_color, status_text) = match &self.load_state {
            WorkspaceLoadState::Ready => (accent_color, "Ready"),
            WorkspaceLoadState::Loading(_) => (warning_color, "Loading..."),
            WorkspaceLoadState::Reconnecting(_) => (warning_color, "Reconnecting..."),
            WorkspaceLoadState::Error(_) => (error_color, "Error"),
        };

        let show_retry = matches!(self.load_state, WorkspaceLoadState::Error(_));

        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .min_h(px(48.0))
            .px(px(16.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .bg(surface_color)
            .border_b_1()
            .border_color(rgb(0x313244))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .w(px(10.0))
                            .h(px(10.0))
                            .rounded(px(5.0))
                            .bg(status_color),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(text_color)
                                    .child(self.connection_details.host_display()),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(muted_color)
                                    .child(status_text),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .when(show_retry, |el| {
                        el.child(
                            div()
                                .id("retry-button")
                                .px(px(12.0))
                                .py(px(6.0))
                                .rounded(px(6.0))
                                .bg(button_bg)
                                .hover(|s| s.bg(button_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.retry_connection(window, cx);
                                }))
                                .child(
                                    div()
                                        .text_size(px(13.0))
                                        .text_color(text_color)
                                        .child("Retry"),
                                ),
                        )
                    })
                    .child(
                        div()
                            .id("disconnect-button")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .bg(button_bg)
                            .hover(|s| s.bg(button_hover))
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.disconnect(window, cx);
                            }))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .text_color(text_color)
                                    .child("Disconnect"),
                            ),
                    ),
            )
    }

    /// Renders a loading/error state
    fn render_loading_state(&self, message: &str, is_error: bool) -> impl IntoElement {
        let bg_color = rgb(0x1e1e2e);
        let text_color = rgb(0xcdd6f4);
        let muted_color = rgb(0xa6adc8);
        let error_color = rgb(0xf38ba8);
        let accent_color = rgb(0x89b4fa);

        let status_color = if is_error { error_color } else { accent_color };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .justify_center()
            .items_center()
            .bg(bg_color)
            .gap(px(16.0))
            .child(
                div()
                    .w(px(48.0))
                    .h(px(48.0))
                    .rounded(px(12.0))
                    .bg(status_color)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(24.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(bg_color)
                            .child(if is_error { "!" } else { "Z" }),
                    ),
            )
            .child(
                div()
                    .text_size(px(16.0))
                    .text_color(if is_error { error_color } else { text_color })
                    .text_center()
                    .max_w(px(300.0))
                    .child(message.to_string()),
            )
            .when(!is_error, |el| {
                el.child(
                    div()
                        .text_size(px(13.0))
                        .text_color(muted_color)
                        .child("Please wait..."),
                )
            })
    }
}

impl EventEmitter<DisconnectRequested> for WorkspaceView {}
impl EventEmitter<WorkspaceReady> for WorkspaceView {}

impl Focusable for WorkspaceView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_color = rgb(0x1e1e2e);

        let content = match &self.load_state {
            WorkspaceLoadState::Ready => {
                // Show "launching workspace" message briefly before root replacement
                self.render_loading_state("Launching workspace...", false)
                    .into_any_element()
            }
            WorkspaceLoadState::Loading(message) | WorkspaceLoadState::Reconnecting(message) => {
                self.render_loading_state(message, false).into_any_element()
            }
            WorkspaceLoadState::Error(message) => {
                self.render_loading_state(message, true).into_any_element()
            }
        };

        div()
            .id("workspace-view-container")
            .flex()
            .flex_col()
            .size_full()
            .bg(bg_color)
            .child(self.render_status_bar(cx))
            .child(content)
    }
}

/// Connection details extracted from RemoteConnectionOptions
#[derive(Clone)]
struct ConnectionDetails {
    host: String,
    username: String,
    port: u16,
    nickname: Option<String>,
}

impl ConnectionDetails {
    fn from_options(options: &RemoteConnectionOptions) -> Self {
        match options {
            RemoteConnectionOptions::Ssh(opts) => Self::from_ssh(opts),
            _ => Self {
                host: "Remote".to_string(),
                username: "unknown".to_string(),
                port: 0,
                nickname: None,
            },
        }
    }

    fn from_ssh(opts: &SshConnectionOptions) -> Self {
        Self {
            host: opts.host.to_string(),
            username: opts
                .username
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            port: opts.port.unwrap_or(22),
            nickname: opts.nickname.clone(),
        }
    }

    fn host_display(&self) -> String {
        if let Some(nickname) = &self.nickname {
            nickname.clone()
        } else if self.port == 22 {
            format!("{}@{}", self.username, self.host)
        } else {
            format!("{}@{}:{}", self.username, self.host, self.port)
        }
    }
}
