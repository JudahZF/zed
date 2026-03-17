//! Connection view for establishing SSH connections to remote servers.
//!
//! This is the main UI shown when the app launches, allowing users to:
//! - Enter SSH connection details (hostname, username, port)
//! - Connect to a remote development server
//! - View the setup tutorial
//! - Access recent connections

use anyhow::Result;
use askpass::EncryptedPassword;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, Task, Window,
    div, img, prelude::*, px,
};
use http_client::HttpClient;
use remote::{
    HostKeyChallenge, HostKeyDecision, RemoteClient, RemoteConnectionOptions, SshConnectionOptions,
    SshPortForwardOption,
};
use std::sync::Arc;
use theme::ActiveTheme;
use util::shell::ShellKind;

use crate::ios_app_version_string;
use crate::mobile_feature_policy::MobileFeaturePolicy;
use crate::persistence::{
    AuthMode, ConnectionDb, ConnectionProfile, ConnectionProfileInput, SessionRestoreState,
    credential_url,
};
use crate::remote_delegate::{HostKeyPromptRequest, IosRemoteClientDelegate};
use crate::text_input::TextInput;

/// Create an HTTP client for iOS
fn create_http_client() -> Arc<dyn HttpClient> {
    Arc::new(reqwest_client::ReqwestClient::new())
}

fn parse_ssh_args(raw: &str) -> Result<Vec<String>, String> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }

    ShellKind::Posix
        .split(raw)
        .ok_or_else(|| "Invalid SSH args: unmatched quotes or escape sequence".to_string())
}

pub(crate) fn parse_port_forward_spec(spec: &str) -> Result<SshPortForwardOption, String> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return Err("Port forward spec cannot be empty".to_string());
    }

    let parts: Vec<_> = trimmed.split(':').collect();
    match parts.as_slice() {
        [local_port, remote_port] => Ok(SshPortForwardOption {
            local_host: None,
            local_port: local_port
                .parse()
                .map_err(|_| format!("Invalid local port in '{trimmed}'"))?,
            remote_host: Some("localhost".to_string()),
            remote_port: remote_port
                .parse()
                .map_err(|_| format!("Invalid remote port in '{trimmed}'"))?,
        }),
        [local_port, remote_host, remote_port] => Ok(SshPortForwardOption {
            local_host: None,
            local_port: local_port
                .parse()
                .map_err(|_| format!("Invalid local port in '{trimmed}'"))?,
            remote_host: Some((*remote_host).to_string()),
            remote_port: remote_port
                .parse()
                .map_err(|_| format!("Invalid remote port in '{trimmed}'"))?,
        }),
        [local_host, local_port, remote_host, remote_port] => Ok(SshPortForwardOption {
            local_host: Some((*local_host).to_string()),
            local_port: local_port
                .parse()
                .map_err(|_| format!("Invalid local port in '{trimmed}'"))?,
            remote_host: Some((*remote_host).to_string()),
            remote_port: remote_port
                .parse()
                .map_err(|_| format!("Invalid remote port in '{trimmed}'"))?,
        }),
        _ => Err(format!(
            "Invalid port forward '{trimmed}'. Use local:remote, local:host:remote, or local_host:local:remote_host:remote"
        )),
    }
}

pub(crate) fn parse_port_forwards(raw: &str) -> Result<Vec<SshPortForwardOption>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .map(parse_port_forward_spec)
        .collect()
}

pub(crate) fn format_port_forwards(forwards: &[SshPortForwardOption]) -> String {
    forwards
        .iter()
        .map(
            |forward| match (&forward.local_host, &forward.remote_host) {
                (Some(local_host), Some(remote_host)) => format!(
                    "{}:{}:{}:{}",
                    local_host, forward.local_port, remote_host, forward.remote_port
                ),
                (None, Some(remote_host)) => {
                    format!(
                        "{}:{}:{}",
                        forward.local_port, remote_host, forward.remote_port
                    )
                }
                _ => format!("{}:{}", forward.local_port, forward.remote_port),
            },
        )
        .collect::<Vec<_>>()
        .join(", ")
}

/// Event emitted when user wants to view the tutorial
pub struct ShowTutorialRequested;

/// Event emitted when connection succeeds
pub struct ConnectionSucceeded {
    pub client: Entity<RemoteClient>,
    pub remote_path: Option<String>,
    pub connection_profile_id: Option<i64>,
}

/// Represents the current connection state
#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionState {
    /// Initial state, ready for input
    Idle,
    /// Currently attempting to connect
    Connecting,
    /// Waiting for the user to trust an unknown host key
    WaitingForHostKey(HostKeyChallenge),
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
    nickname_input: Entity<TextInput>,
    ssh_args_input: Entity<TextInput>,
    port_forwards_input: Entity<TextInput>,
    password_input: Entity<TextInput>,
    remote_path_input: Entity<TextInput>,
    state: ConnectionState,
    status_message: Option<String>,
    recent_connections: Vec<ConnectionProfile>,
    last_session_profile: Option<ConnectionProfile>,
    last_session_restore_state: Option<SessionRestoreState>,
    db: Option<ConnectionDb>,
    focus_handle: FocusHandle,
    host_key_tx: Option<oneshot::Sender<HostKeyDecision>>,
    password_tx: Option<oneshot::Sender<EncryptedPassword>>,
    connection_task: Option<Task<()>>,
    show_advanced_options: bool,
    remember_secret: bool,
    upload_binary_over_ssh: bool,
    pending_secret_for_save: Option<String>,
    pending_host_key_fingerprint: Option<String>,
    auto_restore_attempted: bool,
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

        let nickname_input = cx.new(|cx| TextInput::new("Work Mac", cx).with_label("Nickname"));

        let ssh_args_input =
            cx.new(|cx| TextInput::new("-i ~/.ssh/id_ed25519", cx).with_label("SSH Args"));

        let port_forwards_input =
            cx.new(|cx| TextInput::new("3000:3000, 5173:5173", cx).with_label("Port Forwards"));

        let password_input = cx.new(|cx| {
            TextInput::new("", cx)
                .with_label("Password")
                .with_secure(true)
        });

        let remote_path_input = cx.new(|cx| TextInput::new("~", cx).with_label("Remote Path"));

        let db = ConnectionDb::open()
            .map_err(|e| {
                log::warn!("Failed to open connection database: {}", e);
                e
            })
            .ok();

        let recent_connections = db
            .as_ref()
            .and_then(|db| db.recent_connection_profiles(10).ok())
            .unwrap_or_default();
        let last_session_profile = db
            .as_ref()
            .and_then(|db| db.last_session_connection_profile().ok())
            .flatten();
        let last_session_restore_state = db
            .as_ref()
            .and_then(|db| db.load_session_restore_state().ok())
            .flatten();

        if let Some(profile) = last_session_profile.as_ref() {
            hostname_input.update(cx, |input, cx| {
                input.set_text(profile.hostname.clone(), cx);
            });
            username_input.update(cx, |input, cx| {
                input.set_text(profile.username.clone(), cx);
            });
            port_input.update(cx, |input, cx| {
                input.set_text(profile.port.to_string(), cx);
            });
            nickname_input.update(cx, |input, cx| {
                input.set_text(profile.nickname.clone().unwrap_or_default(), cx);
            });
            remote_path_input.update(cx, |input, cx| {
                input.set_text(
                    last_session_restore_state
                        .as_ref()
                        .and_then(|state| state.remote_path.clone())
                        .or_else(|| profile.default_path.clone())
                        .clone()
                        .unwrap_or_else(|| "~".to_string()),
                    cx,
                );
            });
            ssh_args_input.update(cx, |input, cx| {
                input.set_text(profile.ssh_args.join(" "), cx);
            });
            port_forwards_input.update(cx, |input, cx| {
                input.set_text(format_port_forwards(&profile.port_forwards), cx);
            });
        }

        let remember_secret = last_session_profile
            .as_ref()
            .is_some_and(|profile| matches!(profile.auth_mode, AuthMode::KeychainSecret));
        let upload_binary_over_ssh = last_session_profile
            .as_ref()
            .is_some_and(|profile| profile.upload_binary_over_ssh);

        Self {
            hostname_input,
            username_input,
            port_input,
            nickname_input,
            ssh_args_input,
            port_forwards_input,
            password_input,
            state: ConnectionState::Idle,
            status_message: None,
            recent_connections,
            last_session_profile,
            last_session_restore_state,
            db,
            focus_handle: cx.focus_handle(),
            host_key_tx: None,
            password_tx: None,
            connection_task: None,
            remote_path_input,
            show_advanced_options: false,
            remember_secret,
            upload_binary_over_ssh,
            pending_secret_for_save: None,
            pending_host_key_fingerprint: None,
            auto_restore_attempted: false,
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
            self.recent_connections = db.recent_connection_profiles(10).unwrap_or_default();
            self.last_session_profile = db.last_session_connection_profile().ok().flatten();
            self.last_session_restore_state = db.load_session_restore_state().ok().flatten();
        }
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

    fn connection_profile_input(&self, cx: &App) -> Result<ConnectionProfileInput, String> {
        self.validate_input(cx)?;

        let hostname = self.hostname_input.read(cx).text().trim().to_string();
        let username = self.username_input.read(cx).text().trim().to_string();
        let port = self.port_input.read(cx).text().parse().unwrap_or(22);
        let nickname = self.nickname_input.read(cx).text().trim().to_string();
        let ssh_args = parse_ssh_args(self.ssh_args_input.read(cx).text())?;
        let port_forwards = parse_port_forwards(self.port_forwards_input.read(cx).text())?;
        let default_path = self.connection_path(cx);

        Ok(ConnectionProfileInput {
            hostname,
            username,
            port,
            nickname: (!nickname.is_empty()).then_some(nickname),
            default_path: Some(default_path.clone()),
            ssh_args,
            port_forwards,
            auth_mode: if self.remember_secret {
                AuthMode::KeychainSecret
            } else {
                AuthMode::Prompt
            },
            upload_binary_over_ssh: self.upload_binary_over_ssh,
            last_successful_server_version: Some(ios_app_version_string().to_string()),
            last_opened_worktree: Some(default_path),
        })
    }

    /// Attempts to connect with the current parameters
    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let profile_input = match self.connection_profile_input(cx) {
            Ok(profile_input) => profile_input,
            Err(error) => {
                self.state = ConnectionState::Error(error);
                cx.notify();
                return;
            }
        };

        let connection_options = SshConnectionOptions {
            host: profile_input.hostname.clone().into(),
            username: Some(profile_input.username.clone()),
            port: Some(profile_input.port),
            password: None,
            args: Some(profile_input.ssh_args.clone()),
            port_forwards: (!profile_input.port_forwards.is_empty())
                .then_some(profile_input.port_forwards.clone()),
            connection_timeout: Some(30),
            nickname: profile_input.nickname.clone(),
            upload_binary_over_ssh: profile_input.upload_binary_over_ssh,
        };

        let credential_store_url = credential_url(
            &profile_input.hostname,
            &profile_input.username,
            profile_input.port,
        );
        let should_read_keychain = matches!(profile_input.auth_mode, AuthMode::KeychainSecret);
        let remote_path = profile_input
            .default_path
            .clone()
            .unwrap_or_else(|| "~".to_string());

        self.state = ConnectionState::Connecting;
        self.status_message = Some("Initializing connection...".to_string());
        self.pending_host_key_fingerprint = None;
        cx.notify();

        log::info!("[Zed iOS] Starting remote connection");

        let window_handle = window.window_handle();
        let this_weak = cx.entity().downgrade();
        let http_client = create_http_client();
        let (host_key_prompt_tx, mut host_key_prompt_rx) =
            mpsc::unbounded::<HostKeyPromptRequest>();
        let credential_store_url_for_task = credential_store_url.clone();
        let profile_input_for_task = profile_input.clone();
        let username_for_keychain = profile_input.username.clone();

        cx.spawn(async move |this, cx| {
            while let Some(HostKeyPromptRequest { challenge, tx }) = host_key_prompt_rx.next().await
            {
                let mut tx = Some(tx);
                if this
                    .update(cx, |this, cx| {
                        this.show_host_key_prompt(
                            challenge,
                            tx.take().expect("host key tx present"),
                            cx,
                        );
                    })
                    .is_err()
                {
                    if let Some(tx) = tx.take() {
                        tx.send(HostKeyDecision::Cancel).ok();
                    }
                    break;
                }
            }
        })
        .detach();

        let connection_task = cx.spawn(async move |this, cx| {
            let known_password = if should_read_keychain {
                match cx
                    .update(|cx| cx.read_credentials(&credential_store_url_for_task))
                    .await
                {
                    Ok(Some((_stored_username, secret_bytes))) => String::from_utf8(secret_bytes)
                        .ok()
                        .and_then(|secret| EncryptedPassword::try_from(secret.as_str()).ok()),
                    Ok(None) => None,
                    Err(err) => {
                        log::warn!(
                            "[Zed iOS] Failed to read saved credentials for {}: {}",
                            credential_store_url_for_task,
                            err
                        );
                        None
                    }
                }
            } else {
                None
            };

            let delegate = Arc::new(IosRemoteClientDelegate::new(
                window_handle,
                this_weak,
                known_password,
                host_key_prompt_tx,
                http_client,
            ));

            let result = Self::perform_connection(
                RemoteConnectionOptions::Ssh(connection_options),
                delegate,
                cx,
            )
            .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(client) => {
                        let mut connection_profile_id = None;
                        if let Some(db) = &this.db {
                            match db.upsert_connection_profile(&profile_input_for_task) {
                                Ok(profile) => {
                                    connection_profile_id = Some(profile.id);
                                    if let Some(fingerprint) =
                                        this.pending_host_key_fingerprint.take()
                                        && let Err(err) = db
                                            .record_host_key_verification(profile.id, &fingerprint)
                                    {
                                        log::warn!(
                                            "Failed to persist host key fingerprint for {}: {}",
                                            profile.display_name(),
                                            err
                                        );
                                    }
                                }
                                Err(err) => {
                                    log::warn!(
                                        "Failed to save connection profile for {}@{}: {}",
                                        profile_input_for_task.username,
                                        profile_input_for_task.hostname,
                                        err
                                    );
                                }
                            }
                            this.reload_recent_connections();
                        }

                        if matches!(profile_input_for_task.auth_mode, AuthMode::KeychainSecret) {
                            if let Some(secret) = this.pending_secret_for_save.take() {
                                let write_task = cx.write_credentials(
                                    &credential_store_url,
                                    &username_for_keychain,
                                    secret.as_bytes(),
                                );
                                cx.spawn(async move |_, _| {
                                    if let Err(err) = write_task.await {
                                        log::warn!(
                                            "[Zed iOS] Failed to store SSH secret in Keychain: {}",
                                            err
                                        );
                                    }
                                })
                                .detach();
                            }
                        } else {
                            let delete_task = cx.delete_credentials(&credential_store_url);
                            cx.spawn(async move |_, _| {
                                if let Err(err) = delete_task.await {
                                    log::debug!(
                                        "[Zed iOS] Failed to clear saved SSH secret: {}",
                                        err
                                    );
                                }
                            })
                            .detach();
                        }

                        this.state = ConnectionState::Connected;
                        this.status_message = Some("Connected!".to_string());
                        cx.emit(ConnectionSucceeded {
                            client,
                            remote_path: Some(remote_path.clone()),
                            connection_profile_id,
                        });
                    }
                    Err(e) => {
                        log::error!("[Zed iOS] Connection failed: {:?}", e);
                        this.state = ConnectionState::Error(format!("{:#}", e));
                        this.status_message = None;
                        this.pending_host_key_fingerprint = None;
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
            cx.update(|cx| RemoteClient::new(identifier, connection, cancel_rx, delegate, cx));

        // Await the client task while keeping cancel_tx alive
        let client = client_task.await?;

        // Explicitly drop cancel_tx after the task completes to make the intent clear
        drop(cancel_tx);

        client.ok_or_else(|| anyhow::anyhow!("Failed to create remote client"))
    }

    /// Called by the delegate to show a password prompt
    pub fn show_host_key_prompt(
        &mut self,
        challenge: HostKeyChallenge,
        tx: oneshot::Sender<HostKeyDecision>,
        cx: &mut Context<Self>,
    ) {
        self.state = ConnectionState::WaitingForHostKey(challenge);
        self.host_key_tx = Some(tx);
        cx.notify();
    }

    fn trust_host_key(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(tx), ConnectionState::WaitingForHostKey(challenge)) =
            (self.host_key_tx.take(), self.state.clone())
        {
            self.pending_host_key_fingerprint = Some(challenge.fingerprint_sha256);
            tx.send(HostKeyDecision::TrustAndSave).ok();
            self.state = ConnectionState::Connecting;
            self.status_message = Some("Trusting host key...".to_string());
            cx.notify();
        }
    }

    fn reject_host_key(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tx) = self.host_key_tx.take() {
            tx.send(HostKeyDecision::Cancel).ok();
            self.state = ConnectionState::Error("Host key verification was cancelled.".to_string());
            self.status_message = None;
            self.pending_host_key_fingerprint = None;
            cx.notify();
        }
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
                    if self.remember_secret {
                        self.pending_secret_for_save = Some(password_text);
                    }
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

    fn toggle_advanced_options(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.show_advanced_options = !self.show_advanced_options;
        cx.notify();
    }

    fn toggle_remember_secret(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.remember_secret = !self.remember_secret;
        if !self.remember_secret {
            self.pending_secret_for_save = None;
        }
        cx.notify();
    }

    fn toggle_upload_binary_over_ssh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.upload_binary_over_ssh = !self.upload_binary_over_ssh;
        cx.notify();
    }

    /// Selects a recent connection
    fn select_recent_connection(
        &mut self,
        connection: &ConnectionProfile,
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
            if let Some(path) = &connection.default_path {
                input.set_text(path.clone(), cx);
            } else {
                input.set_text("~", cx);
            }
        });
        self.nickname_input.update(cx, |input, cx| {
            input.set_text(connection.nickname.clone().unwrap_or_default(), cx);
        });
        self.ssh_args_input.update(cx, |input, cx| {
            input.set_text(connection.ssh_args.join(" "), cx);
        });
        self.port_forwards_input.update(cx, |input, cx| {
            input.set_text(format_port_forwards(&connection.port_forwards), cx);
        });
        self.remember_secret = matches!(connection.auth_mode, AuthMode::KeychainSecret);
        self.upload_binary_over_ssh = connection.upload_binary_over_ssh;

        if let Some(db) = &self.db {
            if let Err(e) = db.touch_connection_profile(connection.id) {
                log::warn!("Failed to update connection timestamp: {}", e);
            }
        }

        cx.notify();
    }

    fn resume_last_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.last_session_profile.clone() {
            self.select_recent_connection(&profile, window, cx);
            if self
                .last_session_restore_state
                .as_ref()
                .is_some_and(|state| state.connection_profile_id == profile.id)
            {
                let restore_path = self
                    .last_session_restore_state
                    .as_ref()
                    .and_then(|state| state.remote_path.clone())
                    .unwrap_or_else(|| "~".to_string());
                self.remote_path_input.update(cx, |input, cx| {
                    input.set_text(restore_path, cx);
                });
            }
            self.connect(window, cx);
        }
    }

    pub fn maybe_resume_last_session_automatically(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.auto_restore_attempted {
            return false;
        }
        self.auto_restore_attempted = true;

        let feature_policy = MobileFeaturePolicy::from_app(cx);
        if !feature_policy.restore_last_session {
            return false;
        }

        if let Some(profile) = self.last_session_profile.clone() {
            self.status_message = Some("Restoring your last remote workspace...".to_string());
            self.select_recent_connection(&profile, window, cx);
            if self
                .last_session_restore_state
                .as_ref()
                .is_some_and(|state| state.connection_profile_id == profile.id)
            {
                let restore_path = self
                    .last_session_restore_state
                    .as_ref()
                    .and_then(|state| state.remote_path.clone())
                    .unwrap_or_else(|| "~".to_string());
                self.remote_path_input.update(cx, |input, cx| {
                    input.set_text(restore_path, cx);
                });
            }
            self.connect(window, cx);
            true
        } else {
            false
        }
    }

    pub fn reset_auto_restore_attempt(&mut self) {
        self.auto_restore_attempted = false;
    }

    fn friendly_error_message(message: &str) -> String {
        let lower = message.to_ascii_lowercase();
        if lower.contains("keychanged") || lower.contains("known_hosts") {
            "Host verification failed. Remove the outdated host key entry and reconnect."
                .to_string()
        } else if lower.contains("permission denied")
            || lower.contains("authentication failed")
            || lower.contains("password")
        {
            "Authentication failed. Check your username, password, or SSH key.".to_string()
        } else if lower.contains("failed to open project") || lower.contains("worktree") {
            "Connected to the server, but opening the remote workspace failed.".to_string()
        } else if lower.contains("timed out") || lower.contains("timeout") {
            "Connection timed out. Verify the host, network, and SSH port.".to_string()
        } else {
            message.to_string()
        }
    }

    fn render_header(&self, cx: &App) -> impl IntoElement {
        let colors = cx.theme().colors();
        let text_color = colors.text;
        let muted_color = colors.text_muted;

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                img("images/zed_app_icon.png")
                    .w(px(80.0))
                    .h(px(80.0))
                    .rounded(px(16.0)),
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

    fn render_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        value: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let colors = cx.theme().colors();
        let bg = if value {
            colors.text_accent
        } else {
            colors.element_background
        };
        let fg = if value {
            colors.background
        } else {
            colors.text
        };

        div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(14.0))
            .py(px(12.0))
            .rounded(px(10.0))
            .bg(bg)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _event, window, cx| on_click(this, window, cx)))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(fg)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(fg)
                    .child(if value { "On" } else { "Off" }),
            )
    }

    fn render_connection_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(self.render_toggle(
                "remember-secret-toggle",
                "Remember password in Keychain",
                self.remember_secret,
                cx,
                |this, window, cx| this.toggle_remember_secret(window, cx),
            ))
            .child(
                div()
                    .id("advanced-toggle")
                    .w_full()
                    .px(px(16.0))
                    .py(px(12.0))
                    .rounded(px(10.0))
                    .bg(cx.theme().colors().element_background)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.toggle_advanced_options(window, cx);
                    }))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(cx.theme().colors().text)
                            .text_center()
                            .child(if self.show_advanced_options {
                                "Hide Advanced SSH Options"
                            } else {
                                "Show Advanced SSH Options"
                            }),
                    ),
            )
            .when(self.show_advanced_options, |el| {
                el.child(self.nickname_input.clone())
                    .child(self.ssh_args_input.clone())
                    .child(self.port_forwards_input.clone())
                    .child(self.render_toggle(
                        "upload-binary-toggle",
                        "Upload remote server over SSH",
                        self.upload_binary_over_ssh,
                        cx,
                        |this, window, cx| this.toggle_upload_binary_over_ssh(window, cx),
                    ))
            })
    }

    fn render_status_message(&self, cx: &App) -> impl IntoElement {
        let error_color = cx.theme().status().error;
        let muted_color = cx.theme().colors().text_muted;
        let success_color = cx.theme().status().success;

        // Determine what message to show
        let message_and_color = match &self.state {
            ConnectionState::Error(msg) => Some((Self::friendly_error_message(msg), error_color)),
            ConnectionState::Connecting
            | ConnectionState::WaitingForHostKey(_)
            | ConnectionState::WaitingForPassword(_) => {
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
                .child(self.render_toggle(
                    "password-remember-toggle",
                    "Remember this password",
                    self.remember_secret,
                    cx,
                    |this, window, cx| this.toggle_remember_secret(window, cx),
                ))
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

    fn render_host_key_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let surface_color = colors.elevated_surface_background;
        let text_color = colors.text;
        let muted_color = colors.text_muted;
        let button_color = colors.text_accent;
        let button_hover = colors.element_hover;
        let secondary_color = colors.element_background;

        match &self.state {
            ConnectionState::WaitingForHostKey(challenge) => div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .w_full()
                .max_w(px(540.0))
                .p(px(20.0))
                .rounded(px(12.0))
                .bg(surface_color)
                .child(
                    div()
                        .text_size(px(16.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(text_color)
                        .child("Trust Remote Host Key"),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(muted_color)
                        .child(format!(
                            "The server {}:{} presented an unknown SSH host key.",
                            challenge.host, challenge.port
                        )),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(text_color)
                        .child(format!("Algorithm: {}", challenge.algorithm)),
                )
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(text_color)
                        .child(format!("SHA-256: {}", challenge.fingerprint_sha256)),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(12.0))
                        .child(
                            div()
                                .id("trust-host-key-button")
                                .flex_1()
                                .px(px(20.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .bg(button_color)
                                .hover(|s| s.bg(button_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.trust_host_key(window, cx);
                                }))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(colors.background)
                                        .text_center()
                                        .child("Trust and Continue"),
                                ),
                        )
                        .child(
                            div()
                                .id("cancel-host-key-button")
                                .flex_1()
                                .px(px(20.0))
                                .py(px(12.0))
                                .rounded(px(8.0))
                                .bg(secondary_color)
                                .hover(|s| s.bg(button_hover))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.reject_host_key(window, cx);
                                }))
                                .child(
                                    div()
                                        .text_size(px(14.0))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(text_color)
                                        .text_center()
                                        .child("Cancel"),
                                ),
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
            ConnectionState::Connecting
                | ConnectionState::WaitingForHostKey(_)
                | ConnectionState::WaitingForPassword(_)
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
                            ConnectionState::Connecting
                                | ConnectionState::WaitingForHostKey(_)
                                | ConnectionState::WaitingForPassword(_)
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
            .when(self.last_session_profile.is_some(), |el| {
                el.child(
                    div()
                        .id("resume-last-session-button")
                        .w_full()
                        .px(px(24.0))
                        .py(px(14.0))
                        .rounded(px(10.0))
                        .bg(secondary_color)
                        .hover(|s| s.bg(secondary_hover))
                        .cursor_pointer()
                        .when(is_busy, |el| el.opacity(0.6).cursor_default())
                        .on_click(cx.listener(|this, _event, window, cx| {
                            if !matches!(
                                this.state,
                                ConnectionState::Connecting
                                    | ConnectionState::WaitingForHostKey(_)
                                    | ConnectionState::WaitingForPassword(_)
                            ) {
                                this.resume_last_session(window, cx);
                            }
                        }))
                        .child(
                            div()
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(text_color)
                                .text_center()
                                .child("Resume Last Workspace"),
                        ),
                )
            })
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
                                        .flex()
                                        .flex_col()
                                        .gap(px(4.0))
                                        .child(
                                            div()
                                                .text_size(px(14.0))
                                                .text_color(text_color)
                                                .child(display),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(12.0))
                                                .text_color(muted_color)
                                                .child(
                                                    conn.default_path
                                                        .clone()
                                                        .unwrap_or_else(|| "~".to_string()),
                                                ),
                                        ),
                                )
                        }),
                )
            })
    }
}

impl EventEmitter<ShowTutorialRequested> for ConnectView {}
impl EventEmitter<ConnectionSucceeded> for ConnectView {}

#[cfg(test)]
mod tests {
    use super::ConnectView;

    #[test]
    fn maps_host_verification_errors_to_actionable_copy() {
        let message = ConnectView::friendly_error_message("known_hosts KeyChanged");
        assert!(message.contains("Host verification failed"));
    }

    #[test]
    fn maps_authentication_errors_to_actionable_copy() {
        let message = ConnectView::friendly_error_message("Permission denied (publickey,password)");
        assert!(message.contains("Authentication failed"));
    }
}

impl Focusable for ConnectView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConnectView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg_color = cx.theme().colors().background;
        let is_host_key_prompt = matches!(self.state, ConnectionState::WaitingForHostKey(_));
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
                    .when(!is_host_key_prompt && !is_password_prompt, |el| {
                        el.child(self.render_connection_form(cx))
                    })
                    .child(self.render_status_message(cx))
                    .when(is_host_key_prompt, |el| {
                        el.child(self.render_host_key_prompt(cx))
                    })
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
