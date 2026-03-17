//! Pure-Rust SSH transport for iOS using the `russh` crate.
//!
//! This module provides SSH connectivity without requiring external binaries
//! like OpenSSH, which aren't available on iOS.

use crate::{
    HostKeyChallenge, HostKeyDecision, RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, RemoteConnection, RemoteConnectionOptions},
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
use russh::keys::{PublicKeyBase64, known_hosts::learn_known_hosts_path};
use russh::{ChannelMsg, client};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use tokio::io::AsyncWriteExt as TokioAsyncWriteExt;
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
        let addr_for_connect = addr.clone();
        let username_for_connect = username.clone();
        let host_for_handler = host.clone();
        let port_for_handler = port;
        let delegate_for_connect = delegate.clone();

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
                // Try none authentication
                session
                    .authenticate_none(&username_for_connect)
                    .await
                    .map(|r| r.success())
                    .unwrap_or(false)
            };

            Ok::<_, anyhow::Error>((session, authenticated))
        })?
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
            })?
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
        };

        // Ensure the remote server binary is available
        let (release_channel, version) =
            cx.update(|cx| (ReleaseChannel::global(cx), AppVersion::global(cx)))?;
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
        })?
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
            super::build_remote_server_from_source(&self.ssh_platform, delegate.as_ref(), cx)
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
            ReleaseChannel::Nightly | ReleaseChannel::Dev => {
                Ok::<Option<Version>, anyhow::Error>(None)
            }
            _ => Ok(Some(AppVersion::global(cx))),
        })??;

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
    ) -> Result<CommandTemplate> {
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

        for (k, v) in input_env.iter() {
            // Validate env var name to prevent command injection.
            // Valid identifiers: start with letter or underscore, followed by letters, digits, or underscores.
            let is_valid_env_name = !k.is_empty()
                && k.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

            if !is_valid_env_name {
                anyhow::bail!(
                    "Invalid environment variable name {:?} in build_command: \
                     names must match [A-Za-z_][A-Za-z0-9_]* (shell kind: {:?})",
                    k,
                    self.ssh_shell_kind
                );
            }

            write!(
                exec,
                "{}={} ",
                k,
                self.ssh_shell_kind.try_quote(v).context("shell quoting")?
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
                write!(exec, " {}", &arg)?;
            }
        } else {
            write!(exec, "{} -l", self.ssh_shell)?;
        };

        Ok(CommandTemplate {
            program: "russh-internal".into(),
            args: vec![exec],
            env: Default::default(),
        })
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
