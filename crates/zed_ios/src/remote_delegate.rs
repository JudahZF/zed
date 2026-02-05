//! Remote client delegate implementation for iOS.
//!
//! This module provides the iOS-specific implementation of `RemoteClientDelegate`,
//! which handles password prompts, status updates, and server binary downloads.

use anyhow::{Context as _, Result};
use askpass::EncryptedPassword;
use futures::AsyncReadExt;
use futures::channel::oneshot;
use gpui::{AnyWindowHandle, AsyncApp, Task, WeakEntity};
use http_client::{AsyncBody, HttpClient, RedirectPolicy};
use release_channel::ReleaseChannel;
use remote::{RemoteClientDelegate as RemoteClientDelegateTrait, RemotePlatform};
use semver::Version;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::connect_view::ConnectView;

/// Release asset metadata from cloud.zed.dev
#[derive(Deserialize)]
struct ReleaseAsset {
    url: String,
    #[serde(default)]
    version: String,
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

    log::info!("Fetching remote server release info from: {}", url);

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

/// iOS-specific implementation of `RemoteClientDelegate`.
///
/// This delegate handles:
/// - Password prompts via the ConnectView UI
/// - Status updates displayed in the connection view
/// - Server binary downloads (either from remote or locally)
#[derive(Clone)]
pub struct IosRemoteClientDelegate {
    window: AnyWindowHandle,
    connect_view: WeakEntity<ConnectView>,
    known_password: Option<EncryptedPassword>,
    http_client: Arc<dyn HttpClient>,
}

impl IosRemoteClientDelegate {
    pub fn new(
        window: AnyWindowHandle,
        connect_view: WeakEntity<ConnectView>,
        known_password: Option<EncryptedPassword>,
        http_client: Arc<dyn HttpClient>,
    ) -> Self {
        Self {
            window,
            connect_view,
            known_password,
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

    fn get_download_url(
        &self,
        platform: RemotePlatform,
        release_channel: ReleaseChannel,
        version: Option<Version>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Option<String>>> {
        let os = platform.os.as_str().to_string();
        let arch = platform.arch.as_str().to_string();
        let channel = release_channel.dev_name().to_string();
        let http_client = self.http_client.clone();

        cx.spawn(async move |_cx| {
            let asset =
                fetch_release_asset(&http_client, &channel, version.as_ref(), &os, &arch).await?;

            log::info!("Remote server download URL: {}", asset.url);
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
        let channel = release_channel.dev_name().to_string();
        let http_client = self.http_client.clone();

        cx.spawn(async move |mut cx| {
            this.update_status(Some("Fetching remote server release"), &mut cx);

            let asset =
                fetch_release_asset(&http_client, &channel, version.as_ref(), &os, &arch).await?;

            // Create the download directory structure
            let servers_dir = paths::remote_servers_dir();
            let channel_dir = servers_dir.join(&channel);
            let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
            let version_path = platform_dir.join(format!("{}.gz", asset.version));

            smol::fs::create_dir_all(&platform_dir)
                .await
                .context("Failed to create remote server directory")?;

            // Check if we already have this version
            if smol::fs::metadata(&version_path).await.is_ok() {
                log::info!("Remote server binary already exists at: {:?}", version_path);
                return Ok(version_path);
            }

            this.update_status(Some("Downloading remote server"), &mut cx);
            log::info!("Downloading remote server from: {}", asset.url);

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

            let mut binary_data = Vec::new();
            response
                .body_mut()
                .read_to_end(&mut binary_data)
                .await
                .context("Failed to read remote server binary")?;

            // Write to file
            smol::fs::write(&version_path, &binary_data)
                .await
                .context("Failed to write remote server binary")?;

            this.update_status(Some("Download complete"), &mut cx);
            log::info!("Remote server binary downloaded to: {:?}", version_path);

            Ok(version_path)
        })
    }
}
