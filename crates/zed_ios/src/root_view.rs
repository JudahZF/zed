//! Root view that manages navigation between app screens.
//!
//! Handles switching between ConnectView, TutorialView, and WorkspaceView.

use gpui::{
    div, prelude::*, AnyWindowHandle, App, Context, Entity, FocusHandle, Focusable, IntoElement,
    Render, Window,
};
use remote::RemoteClient;

use crate::connect_view::{ConnectView, ConnectionSucceeded, ShowTutorialRequested};
use crate::tutorial_view::{TutorialDismissed, TutorialView};
use crate::workspace_view::{DisconnectRequested, WorkspaceView};

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
}

impl RootView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connect_view = cx.new(|cx| ConnectView::new(window, cx));
        let window_handle = window.window_handle();

        // Subscribe to tutorial request events
        cx.subscribe(&connect_view, |this, _connect, _event: &ShowTutorialRequested, cx| {
            this.show_tutorial(cx);
        })
        .detach();

        // Subscribe to connection success events
        cx.subscribe(&connect_view, |this, _connect, event: &ConnectionSucceeded, cx| {
            this.on_connection_succeeded(event.client.clone(), cx);
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
        }
    }

    fn on_connection_succeeded(
        &mut self,
        client: Entity<RemoteClient>,
        cx: &mut Context<Self>,
    ) {
        log::info!("[Zed iOS] Connection succeeded, switching to workspace view");

        // Store the client
        self.remote_client = Some(client.clone());

        // Create the workspace view using window handle
        let window_handle = self.window_handle;
        let workspace = window_handle.update(cx, |_, window, cx| {
            cx.new(|cx| WorkspaceView::new(client, window, cx))
        });

        if let Ok(workspace) = workspace {
            // Subscribe to disconnect events
            cx.subscribe(&workspace, |this, _workspace, _event: &DisconnectRequested, cx| {
                this.on_disconnect_requested(cx);
            })
            .detach();

            self.workspace_view = Some(workspace);
            self.current_screen = Screen::Workspace;
        } else {
            log::error!("[Zed iOS] Failed to create workspace view");
        }

        cx.notify();
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
            cx.subscribe(&tutorial, |this, _tutorial, _event: &TutorialDismissed, cx| {
                this.hide_tutorial(cx);
            })
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
