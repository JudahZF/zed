//! Connection view for establishing SSH connections to remote servers.
//!
//! This is the main UI shown when the app launches, allowing users to:
//! - Enter SSH connection details (hostname, username, port)
//! - Connect to a remote development server
//! - View the setup tutorial
//! - Access recent connections

use anyhow::Result;
use askpass::EncryptedPassword;
use futures::channel::oneshot;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Task, Window,
    div, prelude::*, px,
};
use theme::ActiveTheme;
use http_client::HttpClient;
use remote::{RemoteClient, RemoteConnectionOptions, SshConnectionOptions};
use std::sync::Arc;

use crate::persistence::{ConnectionDb, SavedConnection};
use crate::remote_delegate::IosRemoteClientDelegate;
use crate::text_input::TextInput;

/// Create an HTTP client for iOS
fn create_http_client() -> Arc<dyn HttpClient> {
    Arc::new(reqwest_client::ReqwestClient::new())
}

/// Event emitted when user wants to view the tutorial
pub struct ShowTutorialRequested;

/// Event emitted when connection succeeds
pub struct ConnectionSucceeded {
    pub client: Entity<RemoteClient>,
    pub remote_path: Option<String>,
}

/// Represents the current connection state
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    /// Initial state, ready for input
    Idle,
    /// Currently attempting to connect
    Connecting,
    /// Waiting for password input
    WaitingForPassword(String),
    /// Connection failed with error message
    Error(String),
    /// Successfully connected
    Connected,
}

/// The main connection view
pub struct ConnectView {
    hostname_input: Entity<TextInput>,
    username_input: Entity<TextInput>,
    port_input: Entity<TextInput>,
    password_input: Entity<TextInput>,
    remote_path_input: Entity<TextInput>,
    state: ConnectionState,
    status_message: Option<String>,
    recent_connections: Vec<SavedConnection>,
    db: Option<ConnectionDb>,
    focus_handle: FocusHandle,
    password_tx: Option<oneshot::Sender<EncryptedPassword>>,
    connection_task: Option<Task<()>>,
}

impl ConnectView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let hostname_input =
            cx.new(|cx| TextInput::new("hostname.example.com", cx).with_label("Host"));

        let username_input = cx.new(|cx| TextInput::new("username", cx).with_label("Username"));

        let port_input = cx.new(|cx| {
            let mut input = TextInput::new("22", cx).with_label("Port");
            input.set_text("22", cx);
            input
        });

        let password_input = cx.new(|cx| {
            TextInput::new("", cx)
                .with_label("Password")
                .with_secure(true)
        });

        let remote_path_input = cx.new(|cx| {
            TextInput::new("~", cx)
                .with_label("Remote Path")
        });

        let db = ConnectionDb::open()
            .map_err(|e| {
                log::warn!("Failed to open connection database: {}", e);
                e
            })
            .ok();

        let recent_connections = db
            .as_ref()
            .and_then(|db| db.recent_connections(10).ok())
            .unwrap_or_default();

        Self {
            hostname_input,
            username_input,
            port_input,
            password_input,
            state: ConnectionState::Idle,
            status_message: None,
            recent_connections,
            db,
            focus_handle: cx.focus_handle(),
            password_tx: None,
            connection_task: None,
            remote_path_input,
        }
    }

    fn connection_path(&self, cx: &App) -> String {
        let path = self.remote_path_input.read(cx).text().trim().to_string();
        if path.is_empty() {
            "~".to_string()
        } else {
            path
        }
    }

    /// Reload recent connections from the database
    fn reload_recent_connections(&mut self) {
        if let Some(db) = &self.db {
            self.recent_connections = db.recent_connections(10).unwrap_or_default();
        }
    }

    /// Returns the current connection parameters
    #[allow(dead_code)]
    pub fn connection_params(&self, cx: &App) -> (String, String, u16) {
        let hostname = self.hostname_input.read(cx).text().to_string();
        let username = self.username_input.read(cx).text().to_string();
        let port: u16 = self.port_input.read(cx).text().parse().unwrap_or(22);
        (hostname, username, port)
    }

    /// Validates the current input
    fn validate_input(&self, cx: &App) -> Result<(), String> {
        let hostname = self.hostname_input.read(cx).text();
        let username = self.username_input.read(cx).text();
        let port_str = self.port_input.read(cx).text();

        if hostname.trim().is_empty() {
            return Err("Hostname is required".to_string());
        }
        if username.trim().is_empty() {
            return Err("Username is required".to_string());
        }
        if port_str.parse::<u16>().is_err() {
            return Err("Invalid port number".to_string());
        }
        Ok(())
    }

    /// Attempts to connect with the current parameters
    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(error) = self.validate_input(cx) {
            self.state = ConnectionState::Error(error);
            cx.notify();
            return;
        }

        self.state = ConnectionState::Connecting;
        self.status_message = Some("Initializing connection...".to_string());
        cx.notify();

        let (hostname, username, port) = self.connection_params(cx);
        let remote_path = self.connection_path(cx);
        log::info!("[Zed iOS] Connecting to {}@{}:{}", username, hostname, port);

        // Create SSH connection options
        let connection_options = SshConnectionOptions {
            host: hostname.clone().into(),
            username: Some(username.clone()),
            port: Some(port),
            password: None,
            args: None,
            port_forwards: None,
            connection_timeout: Some(30),
            nickname: None,
            upload_binary_over_ssh: false,
        };

        // Create the delegate with HTTP client
        let window_handle = window.window_handle();
        let this_weak = cx.entity().downgrade();
        let http_client = create_http_client();
        let delegate = Arc::new(IosRemoteClientDelegate::new(
            window_handle,
            this_weak,
            None,
            http_client,
        ));

        // Spawn the connection task
        let connection_task = cx.spawn(async move |this, cx| {
            let remote_path = remote_path.clone();
            let result = Self::perform_connection(
                RemoteConnectionOptions::Ssh(connection_options),
                delegate,
                cx,
            )
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(client) => {
                        let path = remote_path.clone();
                        // Save connection only after successful connection
                        if let Some(db) = &this.db {
                            if let Err(e) = db.save_connection(&hostname, &username, port, None, Some(&path)) {
                                log::warn!("Failed to save connection: {}", e);
                            } else {
                                this.reload_recent_connections();
                            }
                        }
                        this.state = ConnectionState::Connected;
                        this.status_message = Some("Connected!".to_string());
                        cx.emit(ConnectionSucceeded {
                            client,
                            remote_path: Some(path),
                        });
                    }
                    Err(e) => {
                        log::error!("[Zed iOS] Connection failed: {:?}", e);
                        this.state = ConnectionState::Error(format!("{:#}", e));
                        this.status_message = None;
                    }
                }
                this.connection_task = None;
                cx.notify();
            })
            .ok();
        });

        self.connection_task = Some(connection_task);
    }

    /// Performs the actual SSH connection
    async fn perform_connection(
        connection_options: RemoteConnectionOptions,
        delegate: Arc<IosRemoteClientDelegate>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Entity<RemoteClient>> {
        use remote::ConnectionIdentifier;

        // Connect to the remote server
        let connection = remote::connect(connection_options, delegate.clone(), cx).await?;

        // Create the RemoteClient
        // Keep cancel_tx alive until RemoteClient::new completes, otherwise the
        // cancellation receiver will resolve immediately and return None.
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let identifier = ConnectionIdentifier::setup();

        let client_task =
            cx.update(|cx| RemoteClient::new(identifier, connection, cancel_rx, delegate, cx))?;

        // Await the client task while keeping cancel_tx alive
        let client = client_task.await?;

        // Explicitly drop cancel_tx after the task completes to make the intent clear
        drop(cancel_tx);

        client.ok_or_else(|| anyhow::anyhow!("Failed to create remote client"))
    }

    /// Called by the delegate to show a password prompt
    pub fn show_password_prompt(
        &mut self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cx: &mut Context<Self>,
    ) {
        self.state = ConnectionState::WaitingForPassword(prompt);
        self.password_tx = Some(tx);
        self.password_input.update(cx, |input, cx| {
            input.set_text("", cx);
        });
        cx.notify();
    }

    /// Called when user submits the password
    fn submit_password(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tx) = self.password_tx.take() {
            let password_text = self.password_input.read(cx).text().to_string();
            match EncryptedPassword::try_from(password_text.as_str()) {
                Ok(encrypted) => {
                    tx.send(encrypted).ok();
                    self.state = ConnectionState::Connecting;
                    self.status_message = Some("Authenticating...".to_string());
                }
                Err(e) => {
                    log::error!("Failed to encrypt password: {:?}", e);
                    self.state = ConnectionState::Error("Failed to process password".to_string());
                    // Connection task may be waiting; it will fail when tx is dropped
                }
            }
            cx.notify();
        }
    }

    /// Called by the delegate to update the connection status
    pub fn set_connection_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.status_message = status;
        cx.notify();
    }

    /// Shows the tutorial view
    fn show_tutorial(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        log::info!("[Zed iOS] Show tutorial requested");
        cx.emit(ShowTutorialRequested);
    }

    /// Selects a recent connection
    fn select_recent_connection(
        &mut self,
        connection: &SavedConnection,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hostname_input.update(cx, |input, cx| {
            input.set_text(connection.hostname.clone(), cx);
        });
        self.username_input.update(cx, |input, cx| {
            input.set_text(connection.username.clone(), cx);
        });
        self.port_input.update(cx, |input, cx| {
            input.set_text(connection.port.to_string(), cx);
        });
        self.remote_path_input.update(cx, |input, cx| {
            if let Some(path) = &connection.path {
                input.set_text(path.clone(), cx);
            } else {
                input.set_text("~", cx);
            }
        });

        if let Some(db) = &self.db {
            if let Err(e) = db.touch_connection(connection.id) {
                log::warn!("Failed to update connection timestamp: {}", e);
            }
        }

        cx.notify();
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accent_color = colors.text_accent;
        let bg_color = colors.background;
        let text_color = colors.text;
        let muted_color = colors.text_muted;

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .w(px(80.0))
                    .h(px(80.0))
                    .rounded(px(16.0))
                    .bg(accent_color)
                    .flex()
                    .justify_center()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(36.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(bg_color)
                            .child("Z"),
                    ),
            )
            .child(
                div()
                    .text_size(px(28.0))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(text_color)
                    .child("Connect to Server"),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .text_color(muted_color)
                    .child("Enter your remote server details"),
            )
    }

    fn render_connection_form(&self, _cx: &App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w_full()
            .max_w(px(540.0))
            .child(self.hostname_input.clone())
            .child(
                div()
                    .flex()
                    .gap(px(12.0))
                    .child(div().flex_1().child(self.username_input.clone()))
                    .child(div().w(px(100.0)).child(self.port_input.clone())),
            )
            .child(self.remote_path_input.clone())
    }

    fn render_status_message(&self, cx: &App) -> impl IntoElement {
        let error_color = cx.theme().status().error;
        let muted_color = cx.theme().colors().text_muted;
        let success_color = cx.theme().status().success;

        // Determine what message to show
        let message_and_color = match &self.state {
            ConnectionState::Error(msg) => Some((msg.clone(), error_color)),
            ConnectionState::Connecting | ConnectionState::WaitingForPassword(_) => {
                self.status_message.clone().map(|msg| (msg, muted_color))
            }
            ConnectionState::Connected => Some(("Connected!".to_string(), success_color)),
            ConnectionState::Idle => self.status_message.clone().map(|msg| (msg, muted_color)),
        };

        div()
            .min_h(px(24.0))
            .when_some(message_and_color, |el, (message, color)| {
                el.child(
                    div()
                        .text_size(px(14.0))
                        .text_color(color)
                        .text_center()
                        .child(message),
                )
            })
    }

    fn render_password_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let surface_color = colors.elevated_surface_background;
        let text_color = colors.text;
        let button_color = colors.text_accent;
        let button_hover = colors.element_hover;
        let bg_color = colors.background;

        match &self.state {
            ConnectionState::WaitingForPassword(prompt) => div()
                .flex()
                .flex_col()
                .gap(px(16.0))
            .w_full()
            .max_w(px(540.0))
            .p(px(20.0))
                .rounded(px(12.0))
                .bg(surface_color)
                .child(
                    div()
                        .text_size(px(14.0))
                        .text_color(text_color)
                        .child(prompt.clone()),
                )
                .child(self.password_input.clone())
                .child(
                    div()
                        .id("submit-password-button")
                        .w_full()
                        .px(px(20.0))
                        .py(px(12.0))
                        .rounded(px(8.0))
                        .bg(button_color)
                        .hover(|s| s.bg(button_hover))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.submit_password(window, cx);
                        }))
                        .child(
                            div()
                                .text_size(px(14.0))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(bg_color)
                                .text_center()
                                .child("Submit"),
                        ),
                ),
            _ => div(),
        }
    }

    fn render_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let button_color = colors.text_accent;
        let button_hover = colors.element_hover;
        let secondary_color = colors.element_background;
        let secondary_hover = colors.element_hover;
        let text_color = colors.text;
        let bg_color = colors.background;

        let is_busy = matches!(
            self.state,
            ConnectionState::Connecting | ConnectionState::WaitingForPassword(_)
        );

        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .w_full()
            .max_w(px(540.0))
            .child(
                div()
                    .id("connect-button")
                    .w_full()
                    .px(px(24.0))
                    .py(px(14.0))
                    .rounded(px(10.0))
                    .bg(button_color)
                    .hover(|s| s.bg(button_hover))
                    .cursor_pointer()
                    .when(is_busy, |el| el.opacity(0.6).cursor_default())
                    .on_click(cx.listener(|this, _event, window, cx| {
                        if !matches!(
                            this.state,
                            ConnectionState::Connecting | ConnectionState::WaitingForPassword(_)
                        ) {
                            this.connect(window, cx);
                        }
                    }))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(bg_color)
                            .text_center()
                            .child(if is_busy { "Connecting..." } else { "Connect" }),
                    ),
            )
            .child(
                div()
                    .id("tutorial-button")
                    .w_full()
                    .px(px(24.0))
                    .py(px(14.0))
                    .rounded(px(10.0))
                    .bg(secondary_color)
                    .hover(|s| s.bg(secondary_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.show_tutorial(window, cx);
                    }))
                    .child(
                        div()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(text_color)
                            .text_center()
                            .child("View Setup Tutorial"),
                    ),
            )
    }

    fn render_recent_connections(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let surface_color = colors.element_background;
        let text_color = colors.text;
        let muted_color = colors.text_muted;
        let hover_color = colors.element_hover;

        let has_recent = !self.recent_connections.is_empty();

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .w_full()
            .max_w(px(540.0))
            .mt(px(24.0))
            .when(has_recent, |el| {
                el.child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(muted_color)
                        .mb(px(4.0))
                        .child("Recent Connections"),
                )
                .children(
                    self.recent_connections
                        .iter()
                        .enumerate()
                        .map(|(idx, conn)| {
                            let display = conn.display_name();
                            let conn_clone = conn.clone();
                            div()
                                .id(format!("recent-{}", idx))
                                .w_full()
                                .px(px(16.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .bg(surface_color)
                                .hover(|s| s.bg(hover_color))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _event, window, cx| {
                                    this.select_recent_connection(&conn_clone, window, cx);
                                }))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .text_color(text_color)
                                        .child(display),
                                )
                        }),
                )
            })
    }
}

impl EventEmitter<ShowTutorialRequested> for ConnectView {}
impl EventEmitter<ConnectionSucceeded> for ConnectView {}

impl Focusable for ConnectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_color = cx.theme().colors().background;
        let is_password_prompt = matches!(self.state, ConnectionState::WaitingForPassword(_));

        div()
            .id("connect-view-scroll")
            .flex()
            .flex_col()
            .size_full()
            .bg(bg_color)
            .px(px(24.0))
            .py(px(32.0))
            .items_center()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(24.0))
                    .py(px(48.0))
                    .w_full()
                    .max_w(px(540.0))
                    .child(self.render_header(cx))
                    .when(!is_password_prompt, |el| {
                        el.child(self.render_connection_form(cx))
                    })
                    .child(self.render_status_message(cx))
                    .when(is_password_prompt, |el| {
                        el.child(self.render_password_prompt(cx))
                    })
                    .when(!is_password_prompt, |el| {
                        el.child(self.render_buttons(cx))
                            .child(self.render_recent_connections(cx))
                    }),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .mb(px(8.0))
                    .text_center()
                    .text_size(px(12.0))
                    .text_color(cx.theme().colors().text_muted)
                    .child("Zed for iPad - Preview"),
            )
    }
}
