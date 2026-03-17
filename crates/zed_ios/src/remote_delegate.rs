//! Remote client delegate implementation for iOS.
//!
//! This module provides the iOS-specific implementation of `RemoteClientDelegate`,
//! which handles password prompts, status updates, and server binary downloads.

use anyhow::{Context as _, Result};
use askpass::EncryptedPassword;
use futures::{
    AsyncReadExt, FutureExt,
    channel::{mpsc, oneshot},
    future::BoxFuture,
};
use gpui::{AnyWindowHandle, AsyncApp, Task, WeakEntity};
use http_client::{AsyncBody, HttpClient, RedirectPolicy};
use release_channel::ReleaseChannel;
use remote::{
    HostKeyChallenge, HostKeyDecision, RemoteClientDelegate as RemoteClientDelegateTrait,
    RemotePlatform,
};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use smol::io::AsyncWriteExt;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::connect_view::ConnectView;

/// Release asset metadata from cloud.zed.dev
#[derive(Deserialize)]
struct ReleaseAsset {
    url: String,
    #[serde(default)]
    version: String,
    #[serde(default, alias = "checksum_sha256", alias = "checksum")]
    sha256: Option<String>,
}

/// Fetches release asset metadata from cloud.zed.dev
async fn fetch_release_asset(
    http_client: &Arc<dyn HttpClient>,
    channel: &str,
    version: Option<&Version>,
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let version_str = version
        .map(|v| {
            let mut v = v.clone();
            v.pre = semver::Prerelease::EMPTY;
            v.build = semver::BuildMetadata::EMPTY;
            v.to_string()
        })
        .unwrap_or_else(|| "latest".to_string());

    let url = format!(
        "https://cloud.zed.dev/releases/{}/{}/asset?os={}&arch={}&asset=zed-remote-server",
        channel, version_str, os, arch
    );

    log::info!("Fetching remote server release metadata (channel={channel}, os={os}, arch={arch})");

    let request = http_client::Request::builder()
        .uri(&url)
        .extension(RedirectPolicy::FollowAll)
        .body(AsyncBody::empty())?;

    let mut response = http_client.send(request).await?;

    if !response.status().is_success() {
        anyhow::bail!("Failed to fetch release info: HTTP {}", response.status());
    }

    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    let asset: ReleaseAsset =
        serde_json::from_slice(&body).context("Failed to parse release asset response")?;

    Ok(asset)
}

const MAX_REMOTE_SERVER_BINARY_BYTES: u64 = 200 * 1024 * 1024;
const UNSIGNED_REMOTE_SERVER_ENV: &str = "ZED_IOS_ALLOW_UNSIGNED_REMOTE_SERVER";

fn remote_server_release_channel(release_channel: ReleaseChannel) -> &'static str {
    match release_channel {
        // Dev builds do not have downloadable release assets on cloud.zed.dev.
        // Preview is the closest published channel for the in-progress iOS app.
        ReleaseChannel::Dev => ReleaseChannel::Preview.dev_name(),
        _ => release_channel.dev_name(),
    }
}

fn can_trust_unsigned_remote_server_asset(asset: &ReleaseAsset) -> bool {
    asset
        .url
        .starts_with("https://github.com/zed-industries/zed/releases/download/")
}

fn normalize_sha256(raw: &str) -> Option<String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.len() == 64 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(normalized)
    } else {
        None
    }
}

fn allow_unsigned_remote_server_download() -> bool {
    std::env::var_os(UNSIGNED_REMOTE_SERVER_ENV).is_some()
}

async fn sha256_for_path(path: &PathBuf) -> Result<String> {
    let mut file = smol::fs::File::open(path)
        .await
        .with_context(|| format!("Failed to open file for checksum: {:?}", path))?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .with_context(|| format!("Failed to read file for checksum: {:?}", path))?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// iOS-specific implementation of `RemoteClientDelegate`.
///
/// This delegate handles:
/// - Password prompts via the ConnectView UI
/// - Status updates displayed in the connection view
/// - Server binary downloads (either from remote or locally)
pub struct HostKeyPromptRequest {
    pub challenge: HostKeyChallenge,
    pub tx: oneshot::Sender<HostKeyDecision>,
}

#[derive(Clone)]
pub struct IosRemoteClientDelegate {
    window: AnyWindowHandle,
    connect_view: WeakEntity<ConnectView>,
    known_password: Option<EncryptedPassword>,
    host_key_prompt_tx: mpsc::UnboundedSender<HostKeyPromptRequest>,
    http_client: Arc<dyn HttpClient>,
}

impl IosRemoteClientDelegate {
    pub fn new(
        window: AnyWindowHandle,
        connect_view: WeakEntity<ConnectView>,
        known_password: Option<EncryptedPassword>,
        host_key_prompt_tx: mpsc::UnboundedSender<HostKeyPromptRequest>,
        http_client: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            window,
            connect_view,
            known_password,
            host_key_prompt_tx,
            http_client,
        }
    }

    fn update_status(&self, status: Option<&str>, cx: &mut AsyncApp) {
        if let Err(e) = self.window.update(cx, |_, _, cx| {
            self.connect_view.update(cx, |view, cx| {
                view.set_connection_status(status.map(|s| s.to_string()), cx);
            })
        }) {
            log::debug!("Failed to update connection status UI: {}", e);
        }
    }
}

impl RemoteClientDelegateTrait for IosRemoteClientDelegate {
    fn ask_password(
        &self,
        prompt: String,
        tx: oneshot::Sender<EncryptedPassword>,
        cx: &mut AsyncApp,
    ) {
        // If we have a known password, use it immediately
        if let Some(password) = self.known_password.clone() {
            tx.send(password).ok();
            return;
        }

        // Otherwise, show the password prompt in the UI
        if let Err(e) = self.window.update(cx, |_, _, cx| {
            self.connect_view.update(cx, |view, cx| {
                view.show_password_prompt(prompt, tx, cx);
            })
        }) {
            log::debug!("Failed to show password prompt UI: {}", e);
        }
    }

    fn set_status(&self, status: Option<&str>, cx: &mut AsyncApp) {
        self.update_status(status, cx)
    }

    fn confirm_host_key(
        &self,
        challenge: HostKeyChallenge,
    ) -> BoxFuture<'static, Result<HostKeyDecision>> {
        let host_key_prompt_tx = self.host_key_prompt_tx.clone();

        async move {
            let (tx, rx) = oneshot::channel();
            host_key_prompt_tx
                .unbounded_send(HostKeyPromptRequest { challenge, tx })
                .map_err(|_| anyhow::anyhow!("Failed to surface host key verification prompt"))?;

            rx.await
                .map_err(|_| anyhow::anyhow!("Host key verification prompt was dismissed"))
        }
        .boxed()
    }

    fn get_download_url(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        let os = platform.os.as_str().to_string();
        let arch = platform.arch.as_str().to_string();
        let channel = remote_server_release_channel(release_channel).to_string();
        let http_client = self.http_client.clone();

        cx.spawn(async move |_cx| {
            let asset =
                fetch_release_asset(&http_client, &channel, version.as_ref(), &os, &arch).await?;

            log::info!("Remote server download URL acquired");
            Ok(Some(asset.url))
        })
    }

    fn download_server_binary_locally(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<PathBuf>> {
        let this = self.clone();
        let os = platform.os.as_str().to_string();
        let arch = platform.arch.as_str().to_string();
        let channel = remote_server_release_channel(release_channel).to_string();
        let http_client = self.http_client.clone();

        cx.spawn(async move |mut cx| {
            this.update_status(Some("Fetching remote server release"), &mut cx);

            let asset =
                fetch_release_asset(&http_client, &channel, version.as_ref(), &os, &arch).await?;
            let expected_sha256 = asset
                .sha256
                .as_deref()
                .and_then(normalize_sha256)
                .or_else(|| {
                    asset
                        .sha256
                        .as_deref()
                        .and_then(|raw| {
                            log::warn!(
                                "Ignoring invalid remote server sha256 checksum from metadata: {raw}"
                            );
                            None
                        })
                });

            let allow_unsigned = allow_unsigned_remote_server_download()
                || (expected_sha256.is_none() && can_trust_unsigned_remote_server_asset(&asset));

            if expected_sha256.is_none() && !allow_unsigned {
                anyhow::bail!(
                    "Remote server metadata is missing a valid SHA-256 checksum; refusing unsigned download. Set {}=1 to override for local development.",
                    UNSIGNED_REMOTE_SERVER_ENV
                );
            }
            if expected_sha256.is_none() {
                log::warn!(
                    "Checksum missing; allowing unsigned remote server download from trusted release URL: {}",
                    asset.url
                );
            }

            // Create the download directory structure
            let servers_dir = paths::remote_servers_dir();
            let channel_dir = servers_dir.join(&channel);
            let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
            let version_path = platform_dir.join(format!("{}.gz", asset.version));

            smol::fs::create_dir_all(&platform_dir)
                .await
                .context("Failed to create remote server directory")?;

            // Check if we already have this version.
            // Treat empty files as invalid and redownload.
            if let Ok(metadata) = smol::fs::metadata(&version_path).await {
                if metadata.len() > 0 {
                    if let Some(expected) = expected_sha256.as_deref() {
                        let existing_sha256 = sha256_for_path(&version_path).await?;
                        if existing_sha256 == expected {
                            log::info!("Using cached remote server binary");
                            return Ok(version_path);
                        }
                        log::warn!(
                            "Cached remote server checksum mismatch; redownloading (expected {}, got {}).",
                            expected,
                            existing_sha256
                        );
                        if let Err(err) = smol::fs::remove_file(&version_path).await {
                            log::debug!(
                                "Failed to remove checksum-mismatched cached binary {:?}: {err}",
                                version_path
                            );
                        }
                    } else {
                        log::info!("Using cached remote server binary (unsigned mode)");
                        return Ok(version_path);
                    }
                } else {
                    log::warn!(
                        "Remote server binary exists but is empty, redownloading: {:?}",
                        version_path
                    );
                    if let Err(err) = smol::fs::remove_file(&version_path).await {
                        log::debug!(
                            "Failed to remove empty cached binary {:?}: {err}",
                            version_path
                        );
                    }
                }
            }

            this.update_status(Some("Downloading remote server"), &mut cx);
            log::info!("Downloading remote server binary");

            // Download the binary
            let request = http_client::Request::builder()
                .uri(&asset.url)
                .extension(RedirectPolicy::FollowAll)
                .body(AsyncBody::empty())?;

            let mut response = http_client.send(request).await?;

            if !response.status().is_success() {
                anyhow::bail!(
                    "Failed to download remote server: HTTP {}",
                    response.status()
                );
            }

            let tmp_path = platform_dir.join(format!(".{}.tmp", Uuid::new_v4()));
            let mut tmp_file = smol::fs::File::create(&tmp_path)
                .await
                .context("Failed to create temporary remote server binary")?;
            let mut hasher = Sha256::new();
            let mut total_bytes = 0u64;
            let mut chunk = [0u8; 64 * 1024];

            loop {
                let read = response
                    .body_mut()
                    .read(&mut chunk)
                    .await
                    .context("Failed to read remote server binary")?;
                if read == 0 {
                    break;
                }
                total_bytes += read as u64;
                anyhow::ensure!(
                    total_bytes <= MAX_REMOTE_SERVER_BINARY_BYTES,
                    "Remote server binary exceeded size limit ({} bytes).",
                    MAX_REMOTE_SERVER_BINARY_BYTES
                );
                hasher.update(&chunk[..read]);
                tmp_file
                    .write_all(&chunk[..read])
                    .await
                    .context("Failed to write remote server binary chunk")?;
            }

            anyhow::ensure!(total_bytes > 0, "Downloaded remote server binary is empty");
            tmp_file
                .flush()
                .await
                .context("Failed to flush temporary remote server binary")?;
            drop(tmp_file);

            let actual_sha256 = hex::encode(hasher.finalize());
            if let Some(expected_sha256) = expected_sha256.as_deref() {
                anyhow::ensure!(
                    actual_sha256 == expected_sha256,
                    "Remote server checksum mismatch (expected {}, got {})",
                    expected_sha256,
                    actual_sha256
                );
            }

            // Write atomically: write temp file first, then rename into place.
            if let Err(err) = smol::fs::rename(&tmp_path, &version_path).await {
                // Best effort cleanup of temp files.
                if let Err(remove_err) = smol::fs::remove_file(&tmp_path).await {
                    log::debug!("Failed to remove temporary file {:?}: {remove_err}", tmp_path);
                }
                return Err(err).context("Failed to atomically install remote server binary");
            }

            this.update_status(Some("Download complete"), &mut cx);
            log::info!("Remote server binary downloaded ({total_bytes} bytes)");

            Ok(version_path)
        })
    }
}
