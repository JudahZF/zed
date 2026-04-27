//! Pure-Rust SSH transport for iOS using the `russh` crate.
//!
//! This module provides SSH connectivity without requiring external binaries
//! like OpenSSH, which aren't available on iOS.

use crate::{
    HostKeyChallenge, HostKeyDecision, RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{
        CommandTemplate, Interactive, RemoteConnection, RemoteConnectionOptions, SshKeyAuth,
    },
    transport::russh_helper::{
        RusshHelperFrame, RusshHelperRequest, RusshWindowSize, build_russh_helper_command_template,
    },
    transport::ssh::SshConnectionOptions,
    transport::{parse_platform, parse_shell},
};
use anyhow::{Context as _, Result, anyhow};
use askpass::IKnowWhatIAmDoingAndIHaveReadTheDocs;
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    FutureExt as _, StreamExt as _,
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
};
use gpui::{App, AsyncApp, Task};
use gpui_tokio::Tokio;
use paths::remote_server_dir_relative;
use prost::Message as ProstMessage;
use release_channel::{AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyBase64, known_hosts::learn_known_hosts_path};
use russh::{ChannelMsg, Pty, client};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{
    mem::size_of,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::{
    io::{
        AsyncRead as TokioAsyncRead, AsyncReadExt as TokioAsyncReadExt,
        AsyncWrite as TokioAsyncWrite, AsyncWriteExt as TokioAsyncWriteExt,
    },
    net::{TcpListener, UnixListener, UnixStream},
    sync::oneshot,
    task::JoinHandle,
};
use util::{
    paths::{PathStyle, RemotePathBuf},
    rel_path::RelPath,
    shell::ShellKind,
};

/// Pure-Rust SSH connection for iOS using the `russh` crate.
pub struct RusshRemoteConnection {
    session: Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
    connection_options: SshConnectionOptions,
    remote_binary_path: Option<Arc<RelPath>>,
    ssh_platform: RemotePlatform,
    ssh_path_style: PathStyle,
    ssh_shell: String,
    ssh_shell_kind: ShellKind,
    ssh_default_system_shell: String,
    killed: Arc<std::sync::atomic::AtomicBool>,
    forwarding_tasks: Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
    helper_temp_dir: Arc<tempfile::TempDir>,
    helper_socket_path: PathBuf,
    helper_server_task: Arc<tokio::sync::Mutex<Option<JoinHandle<Result<()>>>>>,
    helper_connection_tasks: Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
    helper_shutdown_tx: Arc<tokio::sync::Mutex<Option<oneshot::Sender<()>>>>,
}

/// Handler for russh client events.
///
/// This handler implements SSH host key verification using the user's known_hosts file.
/// The verification follows these rules:
/// 1. If the host key matches an entry in known_hosts, accept the connection
/// 2. If the host key differs from a known entry (key changed), reject with error
/// 3. If the host is not in known_hosts, accept and learn the key (TOFU - Trust On First Use)
///
/// Uses the standard ~/.ssh/known_hosts location. On platforms where ~/.ssh may not
/// exist (like iOS), the directory is created automatically when learning a new host key.
struct RusshHandler {
    /// The hostname being connected to (used for known_hosts lookup)
    host: String,
    /// The port being connected to (used for known_hosts lookup)  
    port: u16,
    delegate: Arc<dyn RemoteClientDelegate>,
}

impl RusshHandler {
    fn new(host: String, port: u16, delegate: Arc<dyn RemoteClientDelegate>) -> Self {
        Self {
            host,
            port,
            delegate,
        }
    }

    /// Get the path to the known_hosts file.
    /// Uses the standard ~/.ssh/known_hosts location.
    fn known_hosts_path() -> Option<std::path::PathBuf> {
        home::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
    }

    /// Ensure the parent directory for known_hosts exists.
    fn ensure_ssh_dir_exists(known_hosts_path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = known_hosts_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        Ok(())
    }

    fn challenge_for_key(
        host: &str,
        port: u16,
        public_key: &russh::keys::PublicKey,
    ) -> HostKeyChallenge {
        let fingerprint_sha256 = hex::encode(Sha256::digest(public_key.public_key_bytes()));
        HostKeyChallenge {
            host: host.to_string(),
            port,
            algorithm: public_key.algorithm().to_string(),
            fingerprint_sha256,
        }
    }

    async fn confirm_host_key(
        delegate: &Arc<dyn RemoteClientDelegate>,
        challenge: HostKeyChallenge,
    ) -> Result<HostKeyDecision, russh::Error> {
        delegate.confirm_host_key(challenge).await.map_err(|error| {
            log::warn!("Host key confirmation failed: {error:#}");
            russh::Error::Disconnect
        })
    }
}

impl client::Handler for RusshHandler {
    type Error = russh::Error;

    /// Verify the server's host key against known_hosts.
    ///
    /// This method implements Trust On First Use (TOFU) semantics:
    /// - If the key matches known_hosts: Accept (return Ok(true))
    /// - If the key differs from known_hosts: Reject with KeyChanged error
    /// - If the host is unknown: Accept and save the key for future connections
    ///
    /// This provides security against man-in-the-middle attacks for hosts that
    /// have been connected to before, while allowing first-time connections
    /// to proceed smoothly.
    fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKey,
    ) -> impl std::future::Future<Output = Result<bool, Self::Error>> + Send {
        let host = self.host.clone();
        let port = self.port;
        let pubkey = server_public_key.clone();
        let delegate = self.delegate.clone();

        async move {
            // Get the known_hosts file path
            let Some(known_hosts_path) = Self::known_hosts_path() else {
                // No home directory found - accept the key but log a warning
                log::warn!(
                    "Could not determine home directory for known_hosts; accepting key without verification"
                );
                return Ok(true);
            };

            // Check if the known_hosts file exists
            if !known_hosts_path.exists() {
                let challenge = Self::challenge_for_key(&host, port, &pubkey);
                match Self::confirm_host_key(&delegate, challenge).await? {
                    HostKeyDecision::TrustAndSave => {
                        log::info!("Trusting first-seen host key for {}:{}", host, port);
                        if let Err(e) = Self::ensure_ssh_dir_exists(&known_hosts_path) {
                            log::warn!("Failed to create .ssh directory: {}", e);
                        }
                        if let Err(e) =
                            learn_known_hosts_path(&host, port, &pubkey, &known_hosts_path)
                        {
                            log::warn!("Failed to save host key to known_hosts: {}", e);
                        }
                        return Ok(true);
                    }
                    HostKeyDecision::Cancel => {
                        return Ok(false);
                    }
                }
            }

            // Check the server key against known_hosts
            match russh::keys::check_known_hosts_path(&host, port, &pubkey, &known_hosts_path) {
                Ok(true) => {
                    // Key matches known_hosts - accept
                    log::info!(
                        "Host key for {}:{} verified against known_hosts",
                        host,
                        port
                    );
                    Ok(true)
                }
                Ok(false) => {
                    let challenge = Self::challenge_for_key(&host, port, &pubkey);
                    match Self::confirm_host_key(&delegate, challenge).await? {
                        HostKeyDecision::TrustAndSave => {
                            log::info!(
                                "Host {}:{} not in known_hosts; user approved trust",
                                host,
                                port
                            );
                            if let Err(e) =
                                learn_known_hosts_path(&host, port, &pubkey, &known_hosts_path)
                            {
                                log::warn!("Failed to save host key to known_hosts: {}", e);
                            }
                            Ok(true)
                        }
                        HostKeyDecision::Cancel => Ok(false),
                    }
                }
                Err(russh::keys::Error::KeyChanged { line }) => {
                    // SECURITY: The host key has changed! This could indicate a MITM attack.
                    log::error!(
                        "SECURITY WARNING: Host key for {}:{} has changed! \
                         Previous key was at line {} in known_hosts. \
                         This could indicate a man-in-the-middle attack. \
                         If you trust this new key, remove line {} from {:?} and reconnect.",
                        host,
                        port,
                        line,
                        line,
                        known_hosts_path
                    );
                    // Reject the connection - the user must manually resolve this
                    Err(russh::Error::Keys(russh::keys::Error::KeyChanged { line }))
                }
                Err(e) => {
                    // Parse or IO error in known_hosts - fail closed to maintain TOFU guarantees.
                    // Accepting on error would effectively be "trust always" which undermines security.
                    log::error!(
                        "Failed to verify host key for {}:{} due to known_hosts error: {}. \
                         Connection rejected. Please check your known_hosts file at {:?}",
                        host,
                        port,
                        e,
                        known_hosts_path
                    );
                    Err(russh::Error::Keys(e))
                }
            }
        }
    }
}

impl RusshRemoteConnection {
    fn create_helper_temp_dir() -> Result<tempfile::TempDir> {
        tempfile::Builder::new()
            .prefix("zed-russh-")
            .tempdir_in("/tmp")
            .or_else(|_| tempfile::Builder::new().prefix("zed-russh-").tempdir())
            .context("Failed to create Russh helper temporary directory")
    }

    fn default_helper_window_size() -> RusshWindowSize {
        RusshWindowSize {
            columns: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    async fn read_helper_payload<S>(stream: &mut S) -> Result<Vec<u8>>
    where
        S: TokioAsyncRead + Unpin,
    {
        let mut length_buffer = [0; size_of::<u32>()];
        stream.read_exact(&mut length_buffer).await?;
        let payload_length = u32::from_le_bytes(length_buffer) as usize;
        let mut payload = vec![0; payload_length];
        stream.read_exact(&mut payload).await?;
        Ok(payload)
    }

    async fn write_helper_payload<S>(stream: &mut S, payload: &[u8]) -> Result<()>
    where
        S: TokioAsyncWrite + Unpin,
    {
        let payload_length =
            u32::try_from(payload.len()).context("Russh helper payload exceeds u32")?;
        stream.write_all(&payload_length.to_le_bytes()).await?;
        stream.write_all(payload).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn read_helper_request<S>(stream: &mut S) -> Result<RusshHelperRequest>
    where
        S: TokioAsyncRead + Unpin,
    {
        RusshHelperRequest::decode(&Self::read_helper_payload(stream).await?)
    }

    async fn read_helper_frame<S>(stream: &mut S) -> Result<RusshHelperFrame>
    where
        S: TokioAsyncRead + Unpin,
    {
        RusshHelperFrame::decode(&Self::read_helper_payload(stream).await?)
    }

    async fn write_helper_frame<S>(stream: &mut S, frame: &RusshHelperFrame) -> Result<()>
    where
        S: TokioAsyncWrite + Unpin,
    {
        Self::write_helper_payload(stream, &frame.encode()?).await
    }

    async fn send_helper_error<S>(stream: &mut S, error: &anyhow::Error)
    where
        S: TokioAsyncWrite + Unpin,
    {
        if let Err(write_error) =
            Self::write_helper_frame(stream, &RusshHelperFrame::Error(error.to_string())).await
        {
            log::warn!("[iOS SSH] Failed to send helper error frame: {write_error:#}");
        }
    }

    async fn abort_tasks(tasks: &Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>) {
        let handles = {
            let mut guard = tasks.lock().await;
            guard.drain(..).collect::<Vec<_>>()
        };

        for task in handles {
            task.abort();
            let _ = task.await;
        }
    }

    async fn handle_helper_connection(
        session: Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        stream: &mut UnixStream,
    ) -> Result<()> {
        let request = Self::read_helper_request(stream)
            .await
            .context("Failed to read Russh helper request")?;
        let mut channel = {
            let guard = session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| anyhow!("SSH session not available"))?;
            session.channel_open_session().await?
        };

        if request.interactive {
            let window_size = request
                .initial_window_size
                .unwrap_or_else(Self::default_helper_window_size);
            channel
                .request_pty(
                    true,
                    "xterm-256color",
                    window_size.columns as u32,
                    window_size.rows as u32,
                    window_size.pixel_width as u32,
                    window_size.pixel_height as u32,
                    &[(Pty::ECHO, 1)],
                )
                .await
                .context("Failed to request Russh helper PTY")?;
        }

        channel
            .exec(true, request.command.clone())
            .await
            .context("Failed to exec Russh helper command")?;
        let (mut stream_reader, mut stream_writer) = tokio::io::split(stream);

        loop {
            tokio::select! {
                helper_frame = Self::read_helper_frame(&mut stream_reader) => {
                    match helper_frame {
                        Ok(RusshHelperFrame::Stdin(data)) => {
                            if !data.is_empty() {
                                channel.data(&data[..]).await?;
                            }
                        }
                        Ok(RusshHelperFrame::Resize(size)) => {
                            channel
                                .window_change(
                                    size.columns as u32,
                                    size.rows as u32,
                                    size.pixel_width as u32,
                                    size.pixel_height as u32,
                                )
                                .await?;
                        }
                        Ok(RusshHelperFrame::Eof) => {
                            channel.eof().await?;
                        }
                        Ok(frame) => {
                            anyhow::bail!("Unexpected frame from Russh helper child: {frame:?}");
                        }
                        Err(error)
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof) =>
                        {
                            break;
                        }
                        Err(error) => {
                            return Err(error).context("Failed while reading Russh helper input");
                        }
                    }
                }
                channel_message = channel.wait() => {
                    match channel_message {
                        Some(ChannelMsg::Data { data }) => {
                            Self::write_helper_frame(&mut stream_writer, &RusshHelperFrame::Stdout(data.to_vec())).await?;
                        }
                        Some(ChannelMsg::ExtendedData { data, ext }) => {
                            if ext == 1 {
                                Self::write_helper_frame(&mut stream_writer, &RusshHelperFrame::Stderr(data.to_vec())).await?;
                            }
                        }
                        Some(ChannelMsg::ExitStatus { exit_status }) => {
                            let exit_status = i32::try_from(exit_status).unwrap_or(i32::MAX);
                            Self::write_helper_frame(&mut stream_writer, &RusshHelperFrame::ExitStatus(exit_status)).await?;
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            break;
                        }
                        Some(ChannelMsg::WindowAdjusted { .. }) => {}
                        Some(other) => {
                            log::debug!("[iOS SSH] Ignoring helper channel message: {other:?}");
                        }
                    }
                }
            }
        }

        stream_writer.shutdown().await.ok();
        channel.close().await.ok();
        Ok(())
    }

    async fn run_helper_server(
        session: Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        listener: UnixListener,
        helper_connection_tasks: Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
        mut shutdown_rx: oneshot::Receiver<()>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    break;
                }
                accept_result = listener.accept() => {
                    let (mut stream, _) = accept_result.context("Failed to accept Russh helper connection")?;
                    let session = session.clone();
                    let task = tokio::spawn(async move {
                        if let Err(error) = Self::handle_helper_connection(session, &mut stream).await {
                            Self::send_helper_error(&mut stream, &error).await;
                            log::warn!("[iOS SSH] Russh helper connection failed: {error:#}");
                        }
                    });
                    helper_connection_tasks.lock().await.push(task);
                }
            }
        }

        Ok(())
    }

    fn build_exec_command(
        &self,
        input_program: Option<String>,
        input_args: &[String],
        input_env: &HashMap<String, String>,
        working_dir: Option<String>,
    ) -> Result<String> {
        use std::fmt::Write as _;

        let mut exec = String::new();
        if let Some(working_dir) = working_dir {
            let working_dir = RemotePathBuf::new(working_dir, self.ssh_path_style).to_string();

            const TILDE_PREFIX: &str = "~/";
            if working_dir.starts_with(TILDE_PREFIX) {
                let working_dir = working_dir.trim_start_matches("~").trim_start_matches("/");
                write!(
                    exec,
                    "cd \"$HOME/{working_dir}\" {} ",
                    self.ssh_shell_kind.sequential_and_commands_separator()
                )?;
            } else {
                write!(
                    exec,
                    "cd \"{working_dir}\" {} ",
                    self.ssh_shell_kind.sequential_and_commands_separator()
                )?;
            }
        };
        write!(exec, "exec env ")?;

        for (key, value) in input_env {
            let is_valid_env_name = !key.is_empty()
                && key
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
                && key
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_');

            if !is_valid_env_name {
                anyhow::bail!(
                    "Invalid environment variable name {:?} in build_command: \
                     names must match [A-Za-z_][A-Za-z0-9_]* (shell kind: {:?})",
                    key,
                    self.ssh_shell_kind
                );
            }

            write!(
                exec,
                "{}={} ",
                key,
                self.ssh_shell_kind.try_quote(value).context("shell quoting")?
            )?;
        }

        if let Some(input_program) = input_program {
            write!(
                exec,
                "{}",
                self.ssh_shell_kind
                    .try_quote_prefix_aware(&input_program)
                    .context("shell quoting")?
            )?;
            for arg in input_args {
                let arg = self
                    .ssh_shell_kind
                    .try_quote(arg)
                    .context("shell quoting")?;
                write!(exec, " {}", arg)?;
            }
        } else {
            write!(exec, "{} -l", self.ssh_shell)?;
        }

        Ok(exec)
    }

    async fn authenticate_with_key(
        session: &mut client::Handle<RusshHandler>,
        username: &str,
        key_auth: &SshKeyAuth,
    ) -> Result<bool> {
        let private_key =
            russh::keys::decode_secret_key(&key_auth.private_key, key_auth.passphrase.as_deref())
                .with_context(|| {
                let label = key_auth
                    .display_name
                    .as_deref()
                    .unwrap_or("SSH private key");
                format!("Failed to decode {label}")
            })?;

        let private_key = PrivateKeyWithHashAlg::new(
            Arc::new(private_key),
            session.best_supported_rsa_hash().await?.flatten(),
        );

        Ok(session
            .authenticate_publickey(username.to_string(), private_key)
            .await
            .context("Public-key authentication failed")?
            .success())
    }

    fn local_bind_addr(local_host: &str, local_port: u16) -> SocketAddr {
        let ip_address = local_host
            .parse::<IpAddr>()
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        SocketAddr::new(ip_address, local_port)
    }

    async fn abort_forwarding_tasks(
        forwarding_tasks: &Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
    ) {
        let tasks = {
            let mut guard = forwarding_tasks.lock().await;
            guard.drain(..).collect::<Vec<_>>()
        };

        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    async fn bridge_forwarded_stream(
        session: Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        mut stream: tokio::net::TcpStream,
        originator_addr: SocketAddr,
        remote_host: String,
        remote_port: u16,
    ) -> Result<()> {
        let mut channel = {
            let guard = session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| anyhow!("SSH session not available"))?;
            session
                .channel_open_direct_tcpip(
                    remote_host.clone(),
                    remote_port.into(),
                    originator_addr.ip().to_string(),
                    originator_addr.port().into(),
                )
                .await?
        };

        let mut stream_closed = false;
        let mut buffer = vec![0; 64 * 1024];

        loop {
            tokio::select! {
                read_result = stream.read(&mut buffer), if !stream_closed => {
                    match read_result {
                        Ok(0) => {
                            stream_closed = true;
                            channel.eof().await?;
                        }
                        Ok(bytes_read) => channel.data(&buffer[..bytes_read]).await?,
                        Err(error) => return Err(error.into()),
                    }
                }
                channel_message = channel.wait() => {
                    match channel_message {
                        Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                            stream.write_all(&data).await?;
                        }
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                            if !stream_closed {
                                stream.shutdown().await.ok();
                            }
                            break;
                        }
                        Some(ChannelMsg::WindowAdjusted { .. }) => {}
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }

    async fn run_port_forward_listener(
        session: Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        forwarding_tasks: Arc<tokio::sync::Mutex<Vec<JoinHandle<()>>>>,
        listener: TcpListener,
        local_host: String,
        local_port: u16,
        remote_host: String,
        remote_port: u16,
    ) -> Result<()> {
        loop {
            let (stream, originator_addr) = listener.accept().await.with_context(|| {
                format!(
                    "Failed to accept forwarded connection on {}:{}",
                    local_host, local_port
                )
            })?;

            let session = session.clone();
            let remote_host = remote_host.clone();
            let bridge_task = tokio::spawn(async move {
                if let Err(error) = Self::bridge_forwarded_stream(
                    session,
                    stream,
                    originator_addr,
                    remote_host.clone(),
                    remote_port,
                )
                .await
                {
                    log::warn!(
                        "[iOS SSH] Port forward bridge to {}:{} failed: {error:#}",
                        remote_host,
                        remote_port
                    );
                }
            });

            forwarding_tasks.lock().await.push(bridge_task);
        }
    }

    pub async fn new(
        connection_options: SshConnectionOptions,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        delegate.set_status(Some("Connecting via SSH"), cx);

        // Build the address string
        let host = connection_options.host.to_string();
        let port = connection_options.port.unwrap_or(22);
        let addr = format!("{}:{}", host, port);

        log::info!("Connecting to SSH server at {}", addr);

        // Get the username - require it to be provided, don't default to "root"
        let username = connection_options
            .username
            .clone()
            .ok_or_else(|| anyhow::anyhow!("SSH username is required but was not provided"))?;

        // First, try to connect and authenticate without a password (on Tokio)
        let initial_password = connection_options.password.clone();
        let ssh_key_auth = delegate.ssh_key_auth();
        let addr_for_connect = addr.clone();
        let username_for_connect = username.clone();
        let host_for_handler = host.clone();
        let port_for_handler = port;
        let delegate_for_connect = delegate.clone();
        let ssh_key_auth_for_connect = ssh_key_auth.clone();

        let (mut session, mut authenticated) = Tokio::spawn_result(cx, async move {
            let config = client::Config {
                inactivity_timeout: Some(std::time::Duration::from_secs(60)),
                ..Default::default()
            };
            let config = Arc::new(config);
            let handler =
                RusshHandler::new(host_for_handler, port_for_handler, delegate_for_connect);

            let mut session = client::connect(config, &addr_for_connect, handler)
                .await
                .context("Failed to connect to SSH server")?;

            // Try to authenticate
            let authenticated = if let Some(password) = initial_password {
                session
                    .authenticate_password(&username_for_connect, &password)
                    .await
                    .context("Password authentication failed")?
                    .success()
            } else {
                let authenticated = session
                    .authenticate_none(&username_for_connect)
                    .await
                    .map(|r| r.success())
                    .unwrap_or(false);

                if authenticated {
                    true
                } else if let Some(key_auth) = ssh_key_auth_for_connect.as_ref() {
                    Self::authenticate_with_key(&mut session, &username_for_connect, key_auth)
                        .await?
                } else {
                    false
                }
            };

            Ok::<_, anyhow::Error>((session, authenticated))
        })
        .await?;

        // If not authenticated, request password from user
        if !authenticated {
            let (tx, rx) = futures::channel::oneshot::channel();
            delegate.ask_password(format!("Password for {}@{}:", username, host), tx, cx);

            let encrypted_password = rx
                .await
                .map_err(|_| anyhow!("Password prompt was cancelled"))?;

            let password = encrypted_password
                .decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
                .context("Failed to decrypt password")?;

            // Authenticate with password on Tokio
            let username_for_auth = username.clone();
            let (new_session, auth_result) = Tokio::spawn_result(cx, async move {
                let result = session
                    .authenticate_password(&username_for_auth, &password)
                    .await
                    .context("Password authentication failed")?
                    .success();
                Ok::<_, anyhow::Error>((session, result))
            })
            .await?;

            session = new_session;
            authenticated = auth_result;
        }

        if !authenticated {
            anyhow::bail!("Authentication failed");
        }

        log::info!("SSH authentication successful");
        delegate.set_status(Some("Detecting remote platform"), cx);

        // Wrap session in Arc<Mutex> for shared access
        let session = Arc::new(tokio::sync::Mutex::new(Some(session)));

        // Detect if the remote is Windows
        let is_windows = Self::probe_is_windows_tokio(&session, cx).await?;
        log::info!("Remote is windows: {}", is_windows);

        // Get the remote shell
        let ssh_shell = Self::detect_shell_tokio(&session, is_windows, cx).await?;
        log::info!("Remote shell discovered: {}", ssh_shell);

        let ssh_shell_kind = ShellKind::new(&ssh_shell, is_windows);

        // Detect the remote platform
        let ssh_platform = Self::detect_platform_tokio(&session, is_windows, cx).await?;
        log::info!("Remote platform discovered: {:?}", ssh_platform);

        let (ssh_path_style, ssh_default_system_shell) = match ssh_platform.os {
            RemoteOs::Windows => (PathStyle::Windows, ssh_shell.clone()),
            _ => (PathStyle::Posix, String::from("/bin/sh")),
        };

        let mut this = Self {
            session,
            connection_options,
            remote_binary_path: None,
            ssh_platform,
            ssh_path_style,
            ssh_shell,
            ssh_shell_kind,
            ssh_default_system_shell,
            killed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            forwarding_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            helper_temp_dir: Arc::new(Self::create_helper_temp_dir()?),
            helper_socket_path: PathBuf::new(),
            helper_server_task: Arc::new(tokio::sync::Mutex::new(None)),
            helper_connection_tasks: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            helper_shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
        };

        // Ensure the remote server binary is available
        let (release_channel, version) =
            cx.update(|cx| (ReleaseChannel::global(cx), AppVersion::global(cx)));
        log::info!(
            "Ensuring server binary for release_channel={:?}, version={}",
            release_channel,
            version
        );
        let remote_binary_path = this
            .ensure_server_binary(&delegate, release_channel, version, cx)
            .await?;
        log::info!(
            "Remote binary path set to: {}",
            remote_binary_path.display(this.path_style())
        );
        this.remote_binary_path = Some(remote_binary_path);

        let helper_socket_path = this.helper_temp_dir.path().join("helper.sock");
        let helper_listener = UnixListener::bind(&helper_socket_path)
            .with_context(|| format!("Failed to bind Russh helper socket at {}", helper_socket_path.display()))?;
        let (helper_shutdown_tx, helper_shutdown_rx) = oneshot::channel();
        let helper_connection_tasks = this.helper_connection_tasks.clone();
        let helper_session = this.session.clone();
        let tokio_handle = cx.update(|cx| Tokio::handle(cx));
        let helper_server_task = tokio_handle.spawn(async move {
            Self::run_helper_server(
                helper_session,
                helper_listener,
                helper_connection_tasks,
                helper_shutdown_rx,
            )
            .await
        });
        this.helper_socket_path = helper_socket_path;
        *this.helper_server_task.lock().await = Some(helper_server_task);
        *this.helper_shutdown_tx.lock().await = Some(helper_shutdown_tx);

        Ok(this)
    }

    /// Run a command on the remote server and return its output
    async fn run_command(session: &client::Handle<RusshHandler>, command: &str) -> Result<String> {
        let channel = session.channel_open_session().await?;
        Self::run_command_on_channel(channel, command).await
    }

    /// Run a command on an already-opened channel
    async fn run_command_on_channel(
        mut channel: russh::Channel<russh::client::Msg>,
        command: &str,
    ) -> Result<String> {
        channel.exec(true, command).await?;

        let mut output = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status: Option<u32> = None;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    output.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExtendedData { data, ext }) => {
                    // ext == 1 is stderr
                    if ext == 1 {
                        stderr.extend_from_slice(&data);
                    }
                }
                Some(ChannelMsg::ExitStatus {
                    exit_status: status,
                }) => {
                    exit_status = Some(status);
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    break;
                }
                _ => {}
            }
        }

        // Check exit status - non-zero means command failed
        if let Some(status) = exit_status {
            if status != 0 {
                let stderr_str = String::from_utf8_lossy(&stderr);
                let stdout_str = String::from_utf8_lossy(&output);
                anyhow::bail!(
                    "Command failed with exit code {}: stderr={}, stdout={}",
                    status,
                    stderr_str.trim(),
                    stdout_str.trim()
                );
            }
        }

        Ok(String::from_utf8_lossy(&output).to_string())
    }

    async fn probe_is_windows(session: &client::Handle<RusshHandler>) -> bool {
        match Self::run_command(session, "cmd /c ver").await {
            Ok(output) => output.trim().contains("indows"),
            Err(_) => false,
        }
    }

    async fn detect_shell(session: &client::Handle<RusshHandler>, is_windows: bool) -> String {
        if is_windows {
            return "powershell.exe".to_owned();
        }

        const DEFAULT_SHELL: &str = "sh";
        match Self::run_command(session, "sh -c 'echo $SHELL'").await {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::warn!("Failed to detect remote shell: {e}");
                DEFAULT_SHELL.to_owned()
            }
        }
    }

    async fn detect_platform(
        session: &client::Handle<RusshHandler>,
        is_windows: bool,
    ) -> Result<RemotePlatform> {
        if is_windows {
            let output = Self::run_command(session, "cmd /c echo %PROCESSOR_ARCHITECTURE%")
                .await
                .context("Failed to detect Windows architecture")?;

            return Ok(RemotePlatform {
                os: RemoteOs::Windows,
                arch: match output.trim() {
                    "AMD64" => RemoteArch::X86_64,
                    "ARM64" => RemoteArch::Aarch64,
                    arch => anyhow::bail!(
                        "Prebuilt remote servers are not yet available for windows-{arch}"
                    ),
                },
            });
        }

        let output = Self::run_command(session, "uname -sm")
            .await
            .context("Failed to run 'uname -sm' to determine platform")?;
        parse_platform(&output)
    }

    /// Run a command via the shared session, on Tokio runtime
    async fn run_command_tokio(
        session: &Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        command: &str,
        cx: &AsyncApp,
    ) -> Result<String> {
        let session = session.clone();
        let command = command.to_string();
        Tokio::spawn_result(cx, async move {
            // Open the channel while holding the lock, then release it
            let channel = {
                let guard = session.lock().await;
                let session_ref = guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("SSH session not available"))?;
                session_ref.channel_open_session().await?
            };
            // Now run the command without holding the session lock
            Self::run_command_on_channel(channel, &command).await
        })
        .await
    }

    async fn probe_is_windows_tokio(
        session: &Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        cx: &AsyncApp,
    ) -> Result<bool> {
        match Self::run_command_tokio(session, "cmd /c ver", cx).await {
            Ok(output) => Ok(output.trim().contains("indows")),
            Err(_) => Ok(false),
        }
    }

    async fn detect_shell_tokio(
        session: &Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        is_windows: bool,
        cx: &AsyncApp,
    ) -> Result<String> {
        if is_windows {
            return Ok("powershell.exe".to_owned());
        }

        const DEFAULT_SHELL: &str = "sh";
        match Self::run_command_tokio(session, "sh -c 'echo $SHELL'", cx).await {
            Ok(output) => Ok(parse_shell(&output, DEFAULT_SHELL)),
            Err(e) => {
                log::warn!("Failed to detect remote shell: {e}");
                Ok(DEFAULT_SHELL.to_owned())
            }
        }
    }

    async fn detect_platform_tokio(
        session: &Arc<tokio::sync::Mutex<Option<client::Handle<RusshHandler>>>>,
        is_windows: bool,
        cx: &AsyncApp,
    ) -> Result<RemotePlatform> {
        if is_windows {
            let output =
                Self::run_command_tokio(session, "cmd /c echo %PROCESSOR_ARCHITECTURE%", cx)
                    .await
                    .context("Failed to detect Windows architecture")?;

            return Ok(RemotePlatform {
                os: RemoteOs::Windows,
                arch: match output.trim() {
                    "AMD64" => RemoteArch::X86_64,
                    "ARM64" => RemoteArch::Aarch64,
                    arch => anyhow::bail!(
                        "Prebuilt remote servers are not yet available for windows-{arch}"
                    ),
                },
            });
        }

        let output = Self::run_command_tokio(session, "uname -sm", cx)
            .await
            .context("Failed to run 'uname -sm' to determine platform")?;
        parse_platform(&output)
    }

    async fn ensure_server_binary(
        &self,
        delegate: &Arc<dyn RemoteClientDelegate>,
        release_channel: ReleaseChannel,
        version: Version,
        cx: &mut AsyncApp,
    ) -> Result<Arc<RelPath>> {
        // For Nightly, use "latest" as the version since we always want the latest
        // For Dev, use "build" (local development builds)
        // For other channels, use the actual version
        let version_str = match release_channel {
            ReleaseChannel::Dev => "build".to_string(),
            ReleaseChannel::Nightly => "latest".to_string(),
            _ => version.to_string(),
        };
        let binary_name = format!(
            "zed-remote-server-{}-{}{}",
            release_channel.dev_name(),
            version_str,
            if self.ssh_platform.os.is_windows() {
                ".exe"
            } else {
                ""
            }
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::unix(&binary_name).unwrap());

        #[cfg(debug_assertions)]
        if let Some(remote_server_path) =
            super::build_remote_server_from_source(&self.ssh_platform, delegate.as_ref(), false, cx)
                .await?
        {
            let tmp_path = paths::remote_server_dir_relative().join(
                RelPath::unix(&format!(
                    "download-{}-{}",
                    std::process::id(),
                    remote_server_path.file_name().unwrap().to_string_lossy()
                ))
                .unwrap(),
            );
            self.upload_local_server_binary(&remote_server_path, &tmp_path, delegate, cx)
                .await?;
            self.extract_server_binary(&dst_path, &tmp_path, delegate, cx)
                .await?;
            return Ok(dst_path);
        }

        log::debug!(
            "Checking for existing binary at: {}",
            dst_path.display(self.path_style())
        );

        // Check if binary already exists on remote
        {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| anyhow!("SSH session not available"))?;
            let check_cmd = format!("{} version", dst_path.display(self.path_style()));
            log::debug!("Running check command: {}", check_cmd);
            match Self::run_command(session, &check_cmd).await {
                Ok(output) if !output.trim().is_empty() => {
                    log::debug!("Binary exists, version output: {}", output.trim());
                    return Ok(dst_path);
                }
                Ok(_) => {
                    log::debug!("Binary check returned empty output, treating as not found");
                }
                Err(e) => {
                    log::debug!("Binary does not exist or is not executable: {}", e);
                }
            }
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly | ReleaseChannel::Dev => None,
            _ => Some(AppVersion::global(cx)),
        });

        let tmp_path_gz = remote_server_dir_relative().join(
            RelPath::unix(&format!(
                "{}-download-{}.gz",
                binary_name,
                std::process::id()
            ))
            .unwrap(),
        );

        log::debug!(
            "Will download to temp path: {}",
            tmp_path_gz.display(self.path_style())
        );

        // Try to download on the server first
        if !self.connection_options.upload_binary_over_ssh {
            log::debug!("Attempting to download binary directly on server...");
            if let Some(url) = delegate
                .get_download_url(
                    self.ssh_platform,
                    release_channel,
                    wanted_version.clone(),
                    cx,
                )
                .await?
            {
                log::debug!("Got download URL: {}", url);
                match self
                    .download_binary_on_server(&url, &tmp_path_gz, delegate, cx)
                    .await
                {
                    Ok(_) => {
                        log::debug!("Download on server succeeded, extracting...");
                        self.extract_server_binary(&dst_path, &tmp_path_gz, delegate, cx)
                            .await
                            .context("extracting server binary")?;
                        return Ok(dst_path);
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to download binary on server, will try uploading: {e:#}"
                        );
                    }
                }
            } else {
                log::debug!("No download URL returned from delegate");
            }
        } else {
            log::debug!("upload_binary_over_ssh is set, skipping server-side download");
        }

        // Download locally and upload via SFTP
        log::debug!("Downloading binary locally and uploading via SFTP...");
        let src_path = delegate
            .download_server_binary_locally(
                self.ssh_platform,
                release_channel,
                wanted_version.clone(),
                cx,
            )
            .await
            .context("downloading server binary locally")?;

        log::debug!("Downloaded locally to: {:?}", src_path);

        self.upload_local_server_binary(&src_path, &tmp_path_gz, delegate, cx)
            .await
            .context("uploading server binary")?;

        log::debug!("Uploaded to remote, extracting...");
        self.extract_server_binary(&dst_path, &tmp_path_gz, delegate, cx)
            .await
            .context("extracting server binary")?;

        Ok(dst_path)
    }

    async fn download_binary_on_server(
        &self,
        url: &str,
        tmp_path_gz: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH session not available"))?;

        // Create parent directory
        if let Some(parent) = tmp_path_gz.parent() {
            let mkdir_cmd = format!("mkdir -p {}", parent.display(self.path_style()));
            let _ = Self::run_command(session, &mkdir_cmd).await;
        }

        delegate.set_status(Some("Downloading remote development server on host"), cx);

        let timeout = self
            .connection_options
            .connection_timeout
            .unwrap_or(10)
            .to_string();

        // Try curl first
        let curl_cmd = format!(
            "curl -f -L --connect-timeout {} '{}' -o '{}'",
            timeout,
            url,
            tmp_path_gz.display(self.path_style())
        );

        if Self::run_command(session, &curl_cmd).await.is_ok() {
            return Ok(());
        }

        // Fall back to wget
        log::info!("curl failed, trying wget");
        let wget_cmd = format!(
            "wget --connect-timeout {} --tries 1 '{}' -O '{}'",
            timeout,
            url,
            tmp_path_gz.display(self.path_style())
        );

        Self::run_command(session, &wget_cmd)
            .await
            .context("Neither curl nor wget succeeded")?;

        Ok(())
    }

    async fn upload_local_server_binary(
        &self,
        src_path: &Path,
        tmp_path_gz: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        // Create parent directory
        {
            let guard = self.session.lock().await;
            let session = guard
                .as_ref()
                .ok_or_else(|| anyhow!("SSH session not available"))?;
            if let Some(parent) = tmp_path_gz.parent() {
                let mkdir_cmd = format!("mkdir -p {}", parent.display(self.path_style()));
                let _ = Self::run_command(session, &mkdir_cmd).await;
            }
        }

        let file_data = smol::fs::read(src_path).await?;
        let size = file_data.len();

        let t0 = Instant::now();
        delegate.set_status(Some("Uploading remote development server"), cx);
        log::info!(
            "uploading remote development server to {:?} ({}kb)",
            tmp_path_gz,
            size / 1024
        );

        // Upload using SFTP
        self.upload_file_sftp(
            &tmp_path_gz.display(self.path_style()).to_string(),
            &file_data,
        )
        .await
        .context("failed to upload server binary via SFTP")?;

        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn upload_file_sftp(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH session not available"))?;

        // Open SFTP subsystem
        let channel = session.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;

        // Use russh-sftp for file transfer
        let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await?;

        // Create/open the file for writing
        let mut file = sftp
            .create(remote_path)
            .await
            .context("Failed to create remote file")?;

        // Write the data in chunks
        const CHUNK_SIZE: usize = 32768;
        for chunk in data.chunks(CHUNK_SIZE) {
            file.write_all(chunk).await?;
        }
        file.flush().await?;
        file.shutdown().await?;

        sftp.close().await?;
        Ok(())
    }

    async fn extract_server_binary(
        &self,
        dst_path: &RelPath,
        tmp_path: &RelPath,
        delegate: &Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        delegate.set_status(Some("Extracting remote development server"), cx);
        let guard = self.session.lock().await;
        let session = guard
            .as_ref()
            .ok_or_else(|| anyhow!("SSH session not available"))?;

        let server_mode = 0o755;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let dst_path_str = dst_path.display(self.path_style());

        // Shell-escape paths by replacing ' with '\'' (end quote, escaped quote, start quote)
        fn shell_escape(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }

        let script = if let Some(tmp_path_str) = orig_tmp_path.strip_suffix(".gz") {
            format!(
                "gunzip -f {} && chmod {:o} {} && mv {} {}",
                shell_escape(&orig_tmp_path),
                server_mode,
                shell_escape(tmp_path_str),
                shell_escape(tmp_path_str),
                shell_escape(&dst_path_str)
            )
        } else {
            format!(
                "chmod {:o} {} && mv {} {}",
                server_mode,
                shell_escape(&orig_tmp_path),
                shell_escape(&orig_tmp_path),
                shell_escape(&dst_path_str)
            )
        };

        log::info!("Extracting server binary with script: {}", script);
        // Run the script directly - SSH exec runs commands in a shell,
        // so no need for an extra sh -c wrapper
        let result = Self::run_command(session, &script).await;
        log::info!("Extract result: {:?}", result);
        result?;

        // Verify the binary exists and is executable
        let verify_cmd = format!("{} version", shell_escape(&dst_path_str));
        log::info!("Verifying binary with: {}", verify_cmd);
        match Self::run_command(session, &verify_cmd).await {
            Ok(output) => log::info!("Binary verification output: {}", output.trim()),
            Err(e) => log::error!("Binary verification failed: {}", e),
        }

        Ok(())
    }
}

#[async_trait(?Send)]
impl RemoteConnection for RusshRemoteConnection {
    async fn kill(&self) -> Result<()> {
        self.killed.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(shutdown_tx) = self.helper_shutdown_tx.lock().await.take() {
            let _ = shutdown_tx.send(());
        }
        if let Some(helper_server_task) = self.helper_server_task.lock().await.take() {
            match helper_server_task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("[iOS SSH] Russh helper server failed during shutdown: {error:#}"),
                Err(error) if error.is_cancelled() => {}
                Err(error) => log::warn!("[iOS SSH] Russh helper server join failed during shutdown: {error:#}"),
            }
        }
        Self::abort_tasks(&self.helper_connection_tasks).await;
        Self::abort_forwarding_tasks(&self.forwarding_tasks).await;
        if let Some(session) = self.session.lock().await.take() {
            session
                .disconnect(russh::Disconnect::ByApplication, "", "en")
                .await
                .ok();
        }
        Ok(())
    }

    fn has_been_killed(&self) -> bool {
        self.killed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn connection_options(&self) -> RemoteConnectionOptions {
        RemoteConnectionOptions::Ssh(self.connection_options.clone())
    }

    fn shell(&self) -> String {
        self.ssh_shell.clone()
    }

    fn default_system_shell(&self) -> String {
        self.ssh_default_system_shell.clone()
    }

    fn build_command(
        &self,
        input_program: Option<String>,
        input_args: &[String],
        input_env: &HashMap<String, String>,
        working_dir: Option<String>,
        _port_forward: Option<(u16, String, u16)>,
        interactive: Interactive,
    ) -> Result<CommandTemplate> {
        let executable_path =
            std::env::current_exe().context("Failed to determine current executable for Russh helper")?;
        let command = self.build_exec_command(input_program, input_args, input_env, working_dir)?;
        Ok(build_russh_helper_command_template(
            &executable_path,
            &self.helper_socket_path,
            command,
            interactive,
        ))
    }

    fn build_forward_ports_command(
        &self,
        _forwards: Vec<(u16, String, u16)>,
    ) -> Result<CommandTemplate> {
        Ok(CommandTemplate {
            program: "russh-internal-forward".into(),
            args: vec![],
            env: Default::default(),
        })
    }

    fn start_port_forwarding(
        &self,
        forwards: Vec<(String, u16, String, u16)>,
        cx: &App,
    ) -> Task<Result<()>> {
        let session = self.session.clone();
        let forwarding_tasks = self.forwarding_tasks.clone();

        Tokio::spawn_result(cx, async move {
            Self::abort_forwarding_tasks(&forwarding_tasks).await;

            for (local_host, local_port, remote_host, remote_port) in forwards {
                let bind_addr = Self::local_bind_addr(&local_host, local_port);
                let listener = match TcpListener::bind(bind_addr).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        Self::abort_forwarding_tasks(&forwarding_tasks).await;
                        return Err(anyhow::Error::new(error).context(format!(
                            "Failed to bind local port {}:{}",
                            local_host, local_port
                        )));
                    }
                };

                let session = session.clone();
                let forwarding_tasks_for_listener = forwarding_tasks.clone();
                let local_host_for_task = local_host.clone();
                let remote_host_for_task = remote_host.clone();

                let listener_task = tokio::spawn(async move {
                    if let Err(error) = Self::run_port_forward_listener(
                        session,
                        forwarding_tasks_for_listener,
                        listener,
                        local_host_for_task.clone(),
                        local_port,
                        remote_host_for_task.clone(),
                        remote_port,
                    )
                    .await
                    {
                        log::warn!(
                            "[iOS SSH] Port forward listener {}:{} -> {}:{} stopped: {error:#}",
                            local_host_for_task,
                            local_port,
                            remote_host_for_task,
                            remote_port
                        );
                    }
                });

                forwarding_tasks.lock().await.push(listener_task);
            }

            Ok(())
        })
    }

    fn stop_port_forwarding(&self, cx: &App) -> Task<Result<()>> {
        let forwarding_tasks = self.forwarding_tasks.clone();
        Tokio::spawn_result(cx, async move {
            Self::abort_forwarding_tasks(&forwarding_tasks).await;
            Ok(())
        })
    }

    fn upload_directory(
        &self,
        src_path: PathBuf,
        dest_path: RemotePathBuf,
        cx: &App,
    ) -> Task<Result<()>> {
        let session = self.session.clone();
        let dest_path_str = dest_path.to_string();

        cx.background_executor().spawn(async move {
            // Open the channel while holding the lock, then release for transfer
            let channel = {
                let guard = session.lock().await;
                let session_ref = guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("SSH session not available"))?;
                let channel = session_ref.channel_open_session().await?;
                channel.request_subsystem(true, "sftp").await?;
                channel
            };

            let sftp = russh_sftp::client::SftpSession::new(channel.into_stream()).await?;

            // Upload directory recursively
            upload_directory_recursive(&sftp, &src_path, &dest_path_str).await?;

            sftp.close().await?;
            Ok(())
        })
    }

    fn start_proxy(
        &self,
        unique_identifier: String,
        reconnect: bool,
        incoming_tx: UnboundedSender<Envelope>,
        outgoing_rx: UnboundedReceiver<Envelope>,
        connection_activity_tx: Sender<()>,
        delegate: Arc<dyn RemoteClientDelegate>,
        cx: &mut AsyncApp,
    ) -> Task<Result<i32>> {
        delegate.set_status(Some("Starting proxy"), cx);

        let Some(remote_binary_path) = self.remote_binary_path.clone() else {
            return Task::ready(Err(anyhow!("Remote binary path not set")));
        };

        let session = self.session.clone();
        let path_style = self.path_style();

        cx.background_executor().spawn(async move {
            let binary_path = remote_binary_path.display(path_style).to_string();
            let mut command = format!("{} proxy --identifier {}", binary_path, unique_identifier);
            if reconnect {
                command.push_str(" --reconnect");
            }

            log::info!("Starting remote server proxy with command: {}", command);

            // Open the channel while holding the lock, then release for RPC
            let channel = {
                let guard = session.lock().await;
                let session_ref = guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("SSH session not available"))?;
                let channel = session_ref.channel_open_session().await?;
                channel.exec(true, command).await?;
                channel
            };

            log::info!("SSH channel opened for proxy, starting RPC bridge");

            // Bridge the SSH channel to the RPC protocol
            handle_rpc_over_ssh_channel(channel, incoming_tx, outgoing_rx, connection_activity_tx)
                .await
        })
    }

    fn path_style(&self) -> PathStyle {
        self.ssh_path_style
    }

    fn has_wsl_interop(&self) -> bool {
        false
    }
}


async fn upload_directory_recursive(
    sftp: &russh_sftp::client::SftpSession,
    src_path: &Path,
    dest_path: &str,
) -> Result<()> {
    // Create destination directory
    sftp.create_dir(dest_path).await.ok();

    let mut entries = smol::fs::read_dir(src_path).await?;
    while let Some(entry_result) = futures::StreamExt::next(&mut entries).await {
        let entry = entry_result?;
        let path = entry.path();
        let file_name = entry.file_name();
        let dest_file = format!("{}/{}", dest_path, file_name.to_string_lossy());

        if path.is_dir() {
            Box::pin(upload_directory_recursive(sftp, &path, &dest_file)).await?;
        } else {
            let data = smol::fs::read(&path).await?;
            let mut file = sftp.create(&dest_file).await?;
            file.write_all(&data).await?;
            file.shutdown().await?;
        }
    }
    Ok(())
}

async fn handle_rpc_over_ssh_channel(
    mut channel: russh::Channel<russh::client::Msg>,
    incoming_tx: UnboundedSender<Envelope>,
    mut outgoing_rx: UnboundedReceiver<Envelope>,
    mut connection_activity_tx: Sender<()>,
) -> Result<i32> {
    use crate::protocol::MESSAGE_LEN_SIZE;

    let mut pending_data = Vec::new();
    let mut exit_code = None;
    let mut stderr_output = Vec::new();

    log::info!("Starting RPC bridge over SSH channel");

    loop {
        futures::select! {
            // Handle outgoing messages
            outgoing = outgoing_rx.next() => {
                match outgoing {
                    Some(envelope) => {
                        // Serialize the envelope using prost and send it via the channel
                        let data = envelope.encode_to_vec();
                        let len_bytes = (data.len() as u32).to_le_bytes();
                        channel.data(&len_bytes[..]).await?;
                        channel.data(&data[..]).await?;
                    }
                    None => {
                        // Outgoing channel closed, send EOF
                        log::info!("Outgoing channel closed, sending EOF");
                        channel.eof().await?;
                        break;
                    }
                }
            }

            // Handle incoming channel messages
            msg = channel.wait().fuse() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        log::debug!("Received {} bytes of data from remote", data.len());
                        pending_data.extend_from_slice(&data);
                        connection_activity_tx.try_send(()).ok();

                        // Try to parse complete messages from pending_data
                        while pending_data.len() >= MESSAGE_LEN_SIZE {
                            let message_len = u32::from_le_bytes([
                                pending_data[0],
                                pending_data[1],
                                pending_data[2],
                                pending_data[3],
                            ]) as usize;
                            let total_len = MESSAGE_LEN_SIZE + message_len;

                            if pending_data.len() >= total_len {
                                let message_data: Vec<u8> = pending_data.drain(..total_len).collect();
                                let envelope = Envelope::decode(&message_data[MESSAGE_LEN_SIZE..])?;
                                log::debug!("Decoded envelope with id {}", envelope.id);
                                incoming_tx.unbounded_send(envelope).ok();
                            } else {
                                break;
                            }
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, ext }) => {
                        // ext == 1 is stderr
                        if ext == 1 {
                            stderr_output.extend_from_slice(&data);
                            let stderr_str = String::from_utf8_lossy(&data);
                            log::warn!("Remote stderr: {}", stderr_str.trim());
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        log::info!("Remote process exited with status: {}", exit_status);
                        exit_code = Some(exit_status);
                    }
                    Some(ChannelMsg::Eof) => {
                        log::info!("Received EOF from remote");
                        break;
                    }
                    Some(ChannelMsg::Close) => {
                        log::info!("Remote channel closed");
                        break;
                    }
                    None => {
                        log::info!("Channel returned None, closing");
                        break;
                    }
                    other => {
                        log::debug!("Received other channel message: {:?}", other);
                    }
                }
            }
        }
    }

    if !stderr_output.is_empty() {
        let stderr_str = String::from_utf8_lossy(&stderr_output);
        log::warn!("Remote process stderr output: {}", stderr_str);
    }

    let final_exit_code = exit_code.unwrap_or(0) as i32;
    log::info!("RPC bridge finished with exit code: {}", final_exit_code);
    Ok(final_exit_code)
}
