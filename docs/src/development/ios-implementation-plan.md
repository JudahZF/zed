# Zed Mobile (iPad-first) - Status and Remaining Plan

> **Status:** Phases 1-4 are landed in the current tree for iPad-first remote IDE parity; phase 5 now has repeatable build/archive/upload scripts and mainly depends on Apple signing credentials plus real-device validation
> **Deployment target:** iOS 18.0 on iPad
> **Approach:** Remote-first thin client
> **Renderer:** Native iOS Metal backend in GPUI
> **Blade:** Not used on iOS
> **Primary input:** Hardware keyboard, with touch and gesture support

## Overview

This document tracks the current state of the iPad app work and the remaining plan to get it to a polished distributable build.

Product naming currently follows this split:

- Product and roadmap label: `Zed Mobile (iPad-first)`
- Current implementation boundary: `crates/zed_ios`

That split is intentional. The app is currently iPad-only in implementation, so the internal crate, static library, C entry point, logs, and persistence stay iOS-specific until there is real shared cross-mobile code.

The app is intentionally remote-first. The iPad client connects to a machine running `zed-remote-server` over SSH, then boots the normal workspace stack against that remote project. Local project picking remains unsupported; the only local-file prompt currently used on iOS is the narrow SSH private key import flow for remote authentication.

## Current Architecture

```text
┌─────────────────────────────────────────────────────────┐
│                    iPad Client                          │
│  ┌─────────────────────────────────────────────────┐   │
│  │  GPUI iOS platform layer                        │   │
│  │  - UIKit windowing/input                        │   │
│  │  - Native Metal renderer                        │   │
│  │  - CoreText text system                         │   │
│  ├─────────────────────────────────────────────────┤   │
│  │  zed_ios app crate (internal)                   │   │
│  │  - connect flow                                 │   │
│  │  - tutorial flow                                │   │
│  │  - remote workspace bootstrap                   │   │
│  ├─────────────────────────────────────────────────┤   │
│  │  Remote client + workspace stack                │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          │ SSH + existing remote protocol
                          ▼
┌─────────────────────────────────────────────────────────┐
│         Remote Server (user's dev machine)              │
│              zed-remote-server                          │
└─────────────────────────────────────────────────────────┘
```

## Progress Tracker

| Phase | Status | Notes |
|------|--------|-------|
| 1. GPUI iOS platform layer | Complete | iOS platform, windowing, input, display, text, and Metal renderer modules are present |
| 2. iOS app crate | Complete | `crates/zed_ios` exists, is in the workspace, and is linked from the iOS project |
| 3. Connection and tutorial UI | Complete | Connect screen, tutorial, persistence, remote delegate, and workspace bootstrap are implemented |
| 4. Feature adaptation and polish | Complete | iPad workspace chrome, SSH key import, native port forwarding, diagnostics entry points, native prompts, and restore are implemented |
| 5. Build and distribution | In progress | Repeatable simulator, device, archive, and TestFlight upload scripts are in-tree; real signing credentials and device/TestFlight execution remain environment-dependent |

## Landed Work

### Phase 1: GPUI iOS platform layer

The GPUI iOS backend now exists under `crates/gpui/src/platform/ios/` and includes:

- `platform.rs` for the `Platform` implementation
- `window.rs` for window creation and event handling
- `display.rs` and `dispatcher.rs`
- `text_system.rs` and `open_type.rs`
- `metal_renderer.rs`, `metal_atlas.rs`, and `shaders.metal`
- `ffi.rs`, `events.rs`, and `text_input.rs`

Important details:

- iOS uses a native Metal renderer in the iOS platform module.
- Blade is not the renderer on iOS.
- Directory-based local workspace prompts remain blocked on iOS, but file-only prompts now support narrow flows such as SSH private key import.
- Shared prompts can now use native UIKit alerts through `window.rs` instead of always falling back to custom GPUI modal UI.

### Phase 2: iOS app crate and project wiring

The app crate exists at `crates/zed_ios/` and is already part of the workspace. The main pieces are:

- `src/zed_ios.rs` for app initialization and window bootstrapping
- `src/main.rs` for the stub binary entry point used by cargo builds outside the real iOS app launch path
- `ios/Zed/main.m` for the Objective-C entry point
- `ios/project.yml` and `ios/Zed.xcodeproj/` for the iOS project configuration
- `ios/Zed/Info.plist` and `ios/Zed/Entitlements.plist` for app configuration

The Xcode project already:

- targets iOS 18.0
- targets iPad only
- builds and links `libzed_ios.a`
- links the required Apple frameworks for the current stack

This is also the reason the internal target names remain `zed_ios` for now. `Zed Mobile` is the product direction; `zed_ios` is still the truthful implementation name.

### Phase 3: connection flow, tutorial, and workspace bootstrap

The user-facing remote flow is implemented around a remote-first iPad workflow:

- `connect_view.rs` collects host, username, port, SSH auth mode, port forwards, and remote path
- `tutorial_view.rs` renders the setup guide from markdown files under `src/tutorial/`
- `root_view.rs` switches between connect, tutorial, and workspace states
- `workspace_view.rs` creates a remote project and replaces the window root with `workspace::Workspace`
- `remote_delegate.rs` handles password prompts, status updates, and remote server download/caching
- `persistence.rs` stores recent connections in SQLite

Additional landed work in this phase:

- SSH host-key trust and persistence
- Keychain-backed password storage
- Keychain-backed SSH private key storage, clipboard import, and Files-based import
- native iOS port-forward lifecycle using the russh transport instead of shell placeholder commands
- `MobileWorkspaceSnapshotV1` restore state for remote path and iPad chrome state
- iPad workspace chrome with Files, Outline, Git, Terminal, Search, Tasks, Problems, File Finder, Command Palette, and Agent entry points
- visible port-forward status and retry affordances inside the iPad workspace shell

Persistence notes:

- Recent connections are stored in SQLite at `paths::data_dir()/zed_ios_connections.sqlite`
- Passwords and SSH private keys are stored in the platform credential store and referenced by connection metadata in SQLite
- Session restore state stores a versioned `workspace_state_json` snapshot for the active remote workspace

These filenames intentionally stay iOS-specific for now. They should not be renamed to `zed_mobile_*` until there is an actual shared mobile storage layer and an explicit migration plan.

## Current User Experience

The current flow is:

1. Launch the iPad app.
2. Enter SSH details and an optional remote path.
3. View the setup tutorial if needed.
4. Connect to a remote machine.
5. Download or reuse the matching `zed-remote-server` binary as needed.
6. Open the remote path as a project.
7. Replace the root view with the shared workspace UI, keeping an iPad-specific workspace shell with connection state, tool buttons, and shared workspace panels such as Project, Outline, Git, Terminal, and Agent when available.

## Remaining Work

### Phase 4: feature adaptation and product polish

The remaining implementation work is now mostly fit-and-finish rather than missing core IDE surface:

- audit the remaining desktop-first UI for iPad usability, especially around modals, menus, focus behavior, and safe areas
- continue hiding or gating local-only actions and unsupported platform affordances
- validate the full remote editing workflow on real devices and simulator builds
- polish touch, keyboard, reconnection, and error states
- confirm AI, Git, and side panel behavior is sensible in the constrained layout

### Phase 5: build, release, and operationalization

The build and distribution path is now scripted, with the remaining work concentrated in environment-specific signing and release execution:

- provide Apple signing credentials, provisioning, and the final App Store Connect team identifiers
- run the scripted archive/export/upload flow against a real signing setup
- validate install/run behavior on physical iPad hardware
- optionally add CI once Apple credentials and runner strategy are settled

Current scripts:

- `script/build-ios-app simulator` for simulator validation
- `script/build-ios-app device` for generic device compilation without signing
- `script/build-ios-app archive --allow-provisioning-updates ...` for signed release archives
- `script/upload-ios-testflight --archive-path ... --api-key ... --api-issuer ...` for export and TestFlight upload

## Build Prerequisites

Before running `cargo check -p zed_ios --target aarch64-apple-ios-sim` or building the Xcode project:

- make sure `xcode-select` points at a full Xcode installation
- make sure the Apple Metal compiler is available to `xcrun`
- use `script/setup-ios-toolchain` to validate the setup
- use `script/build-ios-app simulator` or `script/build-ios-app device` for repeatable Xcode-side validation
- make sure the Rust iOS targets are installed:
  `rustup target add aarch64-apple-ios-sim aarch64-apple-ios`
- if the Metal compiler is missing on macOS 26 / Xcode 26, run `script/setup-ios-toolchain --install-metal-toolchain`

For release packaging and TestFlight:

1. Create a signed archive:
   `script/build-ios-app archive --allow-provisioning-updates`
2. Export and upload to TestFlight:
   `script/upload-ios-testflight --api-key <key-id> --api-issuer <issuer-id>`

The current simulator/device build failure that mentions a missing Metal toolchain is an environment issue, not a signal that the in-tree iOS port should be replaced.

## Known Gaps and Risks

- A full local `cargo check -p zed_ios` may still depend on Apple tooling being installed correctly, including the Metal toolchain.
- The renderer fallback in `metal_atlas.rs` now degrades safely instead of panicking, but renderer edge cases still need continued attention.
- The app is intentionally remote-only today. Local workspace selection remains out of scope for the current design.
- The historical plan assumed several files and APIs that no longer match the implementation; this document supersedes that earlier draft.
- Android is still a future platform from this repo's perspective. Do not rename internal iOS crates to `zed_mobile` until shared cross-mobile code actually exists.

## Key Files

| Path | Purpose |
|------|---------|
| `crates/gpui/src/platform/ios.rs` | GPUI iOS platform entry point |
| `crates/gpui/src/platform/ios/platform.rs` | `Platform` trait implementation and remote-only file prompt behavior |
| `crates/gpui/src/platform/ios/window.rs` | Window lifecycle, input, and rendering integration |
| `crates/gpui/src/platform/ios/metal_renderer.rs` | Native iOS renderer |
| `crates/zed_ios/src/zed_ios.rs` | App initialization and window bootstrap |
| `crates/zed_ios/src/connect_view.rs` | Remote connection UI |
| `crates/zed_ios/src/tutorial_view.rs` | Tutorial UI |
| `crates/zed_ios/src/workspace_view.rs` | Remote project loading and workspace transition |
| `crates/zed_ios/src/remote_delegate.rs` | Remote client delegate, password prompts, and binary download |
| `crates/zed_ios/src/persistence.rs` | Recent connection persistence |
| `crates/zed_ios/ios/project.yml` | Xcode project definition |
| `crates/zed_ios/ios/Zed/Info.plist` | App metadata and device/orientation configuration |

## References

- GPUI platform abstraction: `crates/gpui/src/platform/`
- iOS GPUI implementation: `crates/gpui/src/platform/ios/`
- iPad app crate: `crates/zed_ios/`
- Remote development crates: `crates/remote/`, `crates/remote_server/`
