//! Root view that manages navigation between app screens.
//!
//! Handles switching between ConnectView, TutorialView, and WorkspaceView.

use std::sync::Arc;

use gpui::{
    AnyWindowHandle, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window,
    div, prelude::*,
};
use remote::RemoteClient;
use workspace::AppState;

use crate::connect_view::{ConnectView, ConnectionSucceeded, ShowTutorialRequested};
use crate::mobile_workspace_chrome::MobileWorkspaceChrome;
use crate::tutorial_view::{TutorialDismissed, TutorialView};
use crate::workspace_view::{DisconnectRequested, WorkspaceReady, WorkspaceView};

/// The current screen being displayed
#[derive(Clone, Debug, PartialEq)]
enum Screen {
    Connect,
    Tutorial,
    Workspace,
}

/// Root view that manages screen transitions
pub struct RootView {
    current_screen: Screen,
    connect_view: Entity<ConnectView>,
    tutorial_view: Option<Entity<TutorialView>>,
    workspace_view: Option<Entity<WorkspaceView>>,
    remote_client: Option<Entity<RemoteClient>>,
    window_handle: AnyWindowHandle,
    focus_handle: FocusHandle,
    /// Keep the AppState alive for the duration of the app.
    /// This is stored here because AppState::set_global only keeps a weak reference.
    #[allow(dead_code)]
    app_state: Arc<AppState>,
    auto_restore_on_activation: bool,
}

impl RootView {
    pub fn new(app_state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_with_auto_restore(app_state, true, window, cx)
    }

    pub fn new_without_auto_restore(
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_auto_restore(app_state, false, window, cx)
    }

    fn new_with_auto_restore(
        app_state: Arc<AppState>,
        auto_restore_on_activation: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connect_view = cx.new(|cx| ConnectView::new(window, cx));
        let window_handle = window.window_handle();

        // Subscribe to tutorial request events
        cx.subscribe(
            &connect_view,
            |this, _connect, _event: &ShowTutorialRequested, cx| {
                this.show_tutorial(cx);
            },
        )
        .detach();

        // Subscribe to connection success events
        cx.subscribe(
            &connect_view,
            |this, _connect, event: &ConnectionSucceeded, cx| {
                this.on_connection_succeeded(
                    event.client.clone(),
                    event.remote_path.clone(),
                    event.connection_profile_id,
                    cx,
                );
            },
        )
        .detach();

        cx.observe_window_activation(window, |this, window, cx| {
            this.on_window_activation_changed(window, cx);
        })
        .detach();

        Self {
            current_screen: Screen::Connect,
            connect_view,
            tutorial_view: None,
            workspace_view: None,
            remote_client: None,
            window_handle,
            focus_handle: cx.focus_handle(),
            app_state,
            auto_restore_on_activation,
        }
    }

    fn on_window_activation_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.auto_restore_on_activation {
            return;
        }

        if window.is_window_active() {
            if self.current_screen != Screen::Connect || self.remote_client.is_some() {
                return;
            }

            let _ = self.connect_view.update(cx, |connect_view, cx| {
                connect_view.maybe_resume_last_session_automatically(window, cx);
            });
        } else {
            let _ = self.connect_view.update(cx, |connect_view, _cx| {
                connect_view.reset_auto_restore_attempt();
            });
        }
    }

    fn on_connection_succeeded(
        &mut self,
        client: Entity<RemoteClient>,
        remote_path: Option<String>,
        connection_profile_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        log::info!("[Zed iOS] Connection succeeded, switching to workspace view");

        // Store the client
        self.remote_client = Some(client.clone());

        // Create the workspace view using window handle
        let window_handle = self.window_handle;
        let workspace = window_handle.update(cx, |_, window, cx| {
            cx.new(|cx| {
                WorkspaceView::new(
                    client,
                    remote_path.clone(),
                    connection_profile_id,
                    window,
                    cx,
                )
            })
        });

        if let Ok(workspace) = workspace {
            // Subscribe to disconnect events
            cx.subscribe(
                &workspace,
                |this, _workspace, _event: &DisconnectRequested, cx| {
                    this.on_disconnect_requested(cx);
                },
            )
            .detach();

            // Subscribe to workspace ready events - this triggers window root replacement
            cx.subscribe(
                &workspace,
                |this, _workspace_view, event: &WorkspaceReady, cx| {
                    this.on_workspace_ready(event, cx);
                },
            )
            .detach();

            self.workspace_view = Some(workspace);
            self.current_screen = Screen::Workspace;
        } else {
            log::error!("[Zed iOS] Failed to create workspace view");
        }

        cx.notify();
    }

    fn on_workspace_ready(&mut self, event: &WorkspaceReady, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Workspace ready, replacing window root");

        let window_handle = self.window_handle;
        let app_state = self.app_state.clone();
        let workspace = event.workspace.clone();
        let remote_client = event.client.clone();
        let connection_profile_id = event.connection_profile_id;
        let remote_path = event.remote_path.clone();

        cx.spawn(async move |_this, cx| {
            log::info!("[Zed iOS] Spawned task: replacing root with MobileWorkspaceChrome");

            let replace_result = window_handle.update(cx, move |_, window, cx| {
                window.replace_root(cx, move |window, cx| {
                    MobileWorkspaceChrome::new(
                        workspace.clone(),
                        remote_client.clone(),
                        connection_profile_id,
                        remote_path.clone(),
                        app_state.clone(),
                        window,
                        cx,
                    )
                });
                window.activate_window();
            });

            match replace_result {
                Ok(_) => log::info!("[Zed iOS] Workspace root installed"),
                Err(err) => log::error!("[Zed iOS] Failed to replace workspace root: {err}"),
            }
        })
        .detach();
    }

    fn on_disconnect_requested(&mut self, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Disconnecting from remote server");

        // Clean up
        self.workspace_view = None;
        self.remote_client = None;

        // Return to connect screen
        self.current_screen = Screen::Connect;
        cx.notify();
    }

    pub fn show_tutorial(&mut self, cx: &mut Context<Self>) {
        if self.tutorial_view.is_none() {
            let tutorial = cx.new(|cx| TutorialView::new(cx));
            cx.subscribe(
                &tutorial,
                |this, _tutorial, _event: &TutorialDismissed, cx| {
                    this.hide_tutorial(cx);
                },
            )
            .detach();
            self.tutorial_view = Some(tutorial);
        }
        self.current_screen = Screen::Tutorial;
        cx.notify();
    }

    pub fn hide_tutorial(&mut self, cx: &mut Context<Self>) {
        self.current_screen = Screen::Connect;
        self.tutorial_view = None;
        cx.notify();
    }
}

impl Focusable for RootView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RootView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        match self.current_screen {
            Screen::Connect => div().size_full().child(self.connect_view.clone()),
            Screen::Tutorial => {
                if let Some(tutorial) = &self.tutorial_view {
                    div().size_full().child(tutorial.clone())
                } else {
                    div().size_full().child(self.connect_view.clone())
                }
            }
            Screen::Workspace => {
                if let Some(workspace) = &self.workspace_view {
                    div().size_full().child(workspace.clone())
                } else {
                    div().size_full().child(self.connect_view.clone())
                }
            }
        }
    }
}
