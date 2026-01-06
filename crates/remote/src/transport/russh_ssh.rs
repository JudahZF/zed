//! Pure-Rust SSH transport for iOS using the `russh` crate.
//!
//! This module provides SSH connectivity without requiring external binaries
//! like OpenSSH, which aren't available on iOS.

use crate::{
    RemoteArch, RemoteClientDelegate, RemoteOs, RemotePlatform,
    remote_client::{CommandTemplate, RemoteConnection, RemoteConnectionOptions},
    transport::{parse_platform, parse_shell},
    transport::ssh::SshConnectionOptions,
};
use anyhow::{Context as _, Result, anyhow};
use askpass::IKnowWhatIAmDoingAndIHaveReadTheDocs;
use async_trait::async_trait;
use collections::HashMap;
use futures::{
    channel::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
    FutureExt as _, StreamExt as _,
};
use gpui::{App, AsyncApp, Task};
use gpui_tokio::Tokio;
use paths::remote_server_dir_relative;
use prost::Message as ProstMessage;
use release_channel::{AppVersion, ReleaseChannel};
use rpc::proto::Envelope;
use russh::{client, ChannelMsg};
use semver::Version;
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

/// Handler for russh client events
struct RusshHandler;

#[async_trait::async_trait]
impl client::Handler for RusshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // For now, accept all server keys (similar to StrictHostKeyChecking=no)
        // TODO: Implement proper host key verification with a known_hosts file
        Ok(true)
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

        // Get the username
        let username = connection_options
            .username
            .clone()
            .unwrap_or_else(|| "root".to_string());

        // First, try to connect and authenticate without a password (on Tokio)
        let initial_password = connection_options.password.clone();
        let addr_for_connect = addr.clone();
        let username_for_connect = username.clone();
        
        let (mut session, mut authenticated) = Tokio::spawn_result(cx, async move {
            let config = client::Config {
                inactivity_timeout: Some(std::time::Duration::from_secs(60)),
                ..Default::default()
            };
            let config = Arc::new(config);
            let handler = RusshHandler;
            
            let mut session = client::connect(config, &addr_for_connect, handler)
                .await
                .context("Failed to connect to SSH server")?;

            // Try to authenticate
            let authenticated = if let Some(password) = initial_password {
                session
                    .authenticate_password(&username_for_connect, &password)
                    .await
                    .context("Password authentication failed")?
            } else {
                // Try none authentication
                session.authenticate_none(&username_for_connect).await.unwrap_or(false)
            };

            Ok::<_, anyhow::Error>((session, authenticated))
        })?.await?;

        // If not authenticated, request password from user
        if !authenticated {
            let (tx, rx) = futures::channel::oneshot::channel();
            delegate.ask_password(
                format!("Password for {}@{}:", username, host),
                tx,
                cx,
            );
            
            let encrypted_password = rx.await
                .map_err(|_| anyhow!("Password prompt was cancelled"))?;
            
            let password = encrypted_password.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
                .context("Failed to decrypt password")?;
            
            // Authenticate with password on Tokio
            let username_for_auth = username.clone();
            let (new_session, auth_result) = Tokio::spawn_result(cx, async move {
                let result = session
                    .authenticate_password(&username_for_auth, &password)
                    .await
                    .context("Password authentication failed")?;
                Ok::<_, anyhow::Error>((session, result))
            })?.await?;
            
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
        log::error!(
            "Ensuring server binary for release_channel={:?}, version={}",
            release_channel,
            version
        );
        let remote_binary_path = this
            .ensure_server_binary(&delegate, release_channel, version, cx)
            .await?;
        log::error!(
            "Remote binary path set to: {}",
            remote_binary_path.display(this.path_style())
        );
        this.remote_binary_path = Some(remote_binary_path);

        Ok(this)
    }

    /// Run a command on the remote server and return its output
    async fn run_command(
        session: &client::Handle<RusshHandler>,
        command: &str,
    ) -> Result<String> {
        let mut channel = session.channel_open_session().await?;
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
                Some(ChannelMsg::ExitStatus { exit_status: status }) => {
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

    async fn detect_shell(
        session: &client::Handle<RusshHandler>,
        is_windows: bool,
    ) -> String {
        if is_windows {
            return "powershell.exe".to_owned();
        }

        const DEFAULT_SHELL: &str = "sh";
        match Self::run_command(session, "sh -c 'echo $SHELL'").await {
            Ok(output) => parse_shell(&output, DEFAULT_SHELL),
            Err(e) => {
                log::error!("Failed to detect remote shell: {e}");
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
            let guard = session.lock().await;
            let session_ref = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
            Self::run_command(session_ref, &command).await
        })?.await
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
                log::error!("Failed to detect remote shell: {e}");
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
            let output = Self::run_command_tokio(session, "cmd /c echo %PROCESSOR_ARCHITECTURE%", cx)
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
            if self.ssh_platform.os.is_windows() { ".exe" } else { "" }
        );
        let dst_path =
            paths::remote_server_dir_relative().join(RelPath::unix(&binary_name).unwrap());

        log::error!(
            "Checking for existing binary at: {}",
            dst_path.display(self.path_style())
        );

        // Check if binary already exists on remote
        {
            let guard = self.session.lock().await;
            let session = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
            let check_cmd = format!("{} version", dst_path.display(self.path_style()));
            log::error!("Running check command: {}", check_cmd);
            match Self::run_command(session, &check_cmd).await {
                Ok(output) if !output.trim().is_empty() => {
                    log::error!("Binary exists, version output: {}", output.trim());
                    return Ok(dst_path);
                }
                Ok(_) => {
                    log::error!("Binary check returned empty output, treating as not found");
                }
                Err(e) => {
                    log::error!("Binary does not exist or is not executable: {}", e);
                }
            }
        }

        let wanted_version = cx.update(|cx| match release_channel {
            ReleaseChannel::Nightly => Ok(None),
            ReleaseChannel::Dev => {
                anyhow::bail!(
                    "ZED_BUILD_REMOTE_SERVER is not set and no remote server exists at ({:?})",
                    dst_path
                )
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

        log::error!(
            "Will download to temp path: {}",
            tmp_path_gz.display(self.path_style())
        );

        // Try to download on the server first
        if !self.connection_options.upload_binary_over_ssh {
            log::error!("Attempting to download binary directly on server...");
            if let Some(url) = delegate
                .get_download_url(
                    self.ssh_platform,
                    release_channel,
                    wanted_version.clone(),
                    cx,
                )
                .await?
            {
                log::error!("Got download URL: {}", url);
                match self
                    .download_binary_on_server(&url, &tmp_path_gz, delegate, cx)
                    .await
                {
                    Ok(_) => {
                        log::error!("Download on server succeeded, extracting...");
                        self.extract_server_binary(&dst_path, &tmp_path_gz, delegate, cx)
                            .await
                            .context("extracting server binary")?;
                        return Ok(dst_path);
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to download binary on server, will try uploading: {e:#}"
                        );
                    }
                }
            } else {
                log::error!("No download URL returned from delegate");
            }
        } else {
            log::error!("upload_binary_over_ssh is set, skipping server-side download");
        }

        // Download locally and upload via SFTP
        log::error!("Downloading binary locally and uploading via SFTP...");
        let src_path = delegate
            .download_server_binary_locally(
                self.ssh_platform,
                release_channel,
                wanted_version.clone(),
                cx,
            )
            .await
            .context("downloading server binary locally")?;
        
        log::error!("Downloaded locally to: {:?}", src_path);
        
        self.upload_local_server_binary(&src_path, &tmp_path_gz, delegate, cx)
            .await
            .context("uploading server binary")?;
        
        log::error!("Uploaded to remote, extracting...");
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
        let session = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;

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
            let session = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
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
        self.upload_file_sftp(&tmp_path_gz.display(self.path_style()).to_string(), &file_data)
            .await
            .context("failed to upload server binary via SFTP")?;

        log::info!("uploaded remote development server in {:?}", t0.elapsed());
        Ok(())
    }

    async fn upload_file_sftp(&self, remote_path: &str, data: &[u8]) -> Result<()> {
        let guard = self.session.lock().await;
        let session = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
        
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
        let session = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;

        let server_mode = 0o755;
        let orig_tmp_path = tmp_path.display(self.path_style());
        let dst_path_str = dst_path.display(self.path_style());

        let script = if let Some(tmp_path_str) = orig_tmp_path.strip_suffix(".gz") {
            format!(
                "gunzip -f '{}' && chmod {:o} '{}' && mv '{}' '{}'",
                orig_tmp_path, server_mode, tmp_path_str, tmp_path_str, dst_path_str
            )
        } else {
            format!(
                "chmod {:o} '{}' && mv '{}' '{}'",
                server_mode, orig_tmp_path, orig_tmp_path, dst_path_str
            )
        };

        log::info!("Extracting server binary with script: {}", script);
        let result = Self::run_command(session, &format!("sh -c '{}'", script)).await;
        log::info!("Extract result: {:?}", result);
        result?;
        
        // Verify the binary exists and is executable
        let verify_cmd = format!("{} version", dst_path_str);
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
        } else {
            write!(
                exec,
                "cd {} ",
                self.ssh_shell_kind.sequential_and_commands_separator()
            )?;
        };
        write!(exec, "exec env ")?;

        for (k, v) in input_env.iter() {
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
                let arg = self.ssh_shell_kind.try_quote(arg).context("shell quoting")?;
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
                let session_ref = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
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

            log::error!("Starting remote server proxy with command: {}", command);

            // Open the channel while holding the lock, then release for RPC
            let channel = {
                let guard = session.lock().await;
                let session_ref = guard.as_ref().ok_or_else(|| anyhow!("SSH session not available"))?;
                let channel = session_ref.channel_open_session().await?;
                channel.exec(true, command).await?;
                channel
            };

            log::error!("SSH channel opened for proxy, starting RPC bridge");

            // Bridge the SSH channel to the RPC protocol
            handle_rpc_over_ssh_channel(
                channel,
                incoming_tx,
                outgoing_rx,
                connection_activity_tx,
            )
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
        log::error!("Remote process stderr output: {}", stderr_str);
    }

    let final_exit_code = exit_code.unwrap_or(0) as i32;
    log::info!("RPC bridge finished with exit code: {}", final_exit_code);
    Ok(final_exit_code)
}
