# Zed Mobile (iPad-first) - Status and Remaining Plan

> **Status:** Phases 1-3 are landed in the current tree; phase 4 is in progress; phase 5 is scaffolded
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

The app is intentionally remote-first. The iPad client connects to a machine running `zed-remote-server` over SSH, then boots the normal workspace stack against that remote project. Local file picking is intentionally unsupported today.

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
| 3. Connection and tutorial UI | Mostly complete | Connect screen, tutorial, persistence, remote delegate, and workspace bootstrap are implemented |
| 4. Feature adaptation and polish | In progress | Remote-only enforcement exists, but desktop UI auditing and end-to-end validation still need work |
| 5. Build and distribution | Scaffolded | Xcode project and Rust static library linkage exist, but signing, CI, bundling, and TestFlight are still open |

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
- Local file prompts return an error and direct the user to remote connection instead.

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

The user-facing remote flow is largely implemented:

- `connect_view.rs` collects host, username, port, password, and remote path
- `tutorial_view.rs` renders the setup guide from markdown files under `src/tutorial/`
- `root_view.rs` switches between connect, tutorial, and workspace states
- `workspace_view.rs` creates a remote project and replaces the window root with `workspace::Workspace`
- `remote_delegate.rs` handles password prompts, status updates, and remote server download/caching
- `persistence.rs` stores recent connections in SQLite

Persistence notes:

- Recent connections are stored in SQLite at `paths::data_dir()/zed_ios_connections.sqlite`
- This is not the Keychain-based design originally sketched in the draft plan
- Password prompting exists, but long-lived credential storage should still be treated as follow-up work unless explicitly finished and validated

These filenames intentionally stay iOS-specific for now. They should not be renamed to `zed_mobile_*` until there is an actual shared mobile storage layer and an explicit migration plan.

## Current User Experience

The current flow is:

1. Launch the iPad app.
2. Enter SSH details and an optional remote path.
3. View the setup tutorial if needed.
4. Connect to a remote machine.
5. Download or reuse the matching `zed-remote-server` binary as needed.
6. Open the remote path as a project.
7. Replace the root view with the shared workspace UI, keeping only a thin iOS status header with connection state and a Disconnect button while attaching panels such as Project, Git, and Agents when available.

## Remaining Work

### Phase 4: feature adaptation and product polish

The remaining implementation work is mostly about fit-and-finish rather than greenfield architecture:

- audit desktop-first UI for iPad usability, especially around panels, menus, focus behavior, and safe areas
- continue hiding or gating local-only actions and unsupported platform affordances
- validate the full remote editing workflow on real devices and simulator builds
- polish touch, keyboard, reconnection, and error states
- confirm AI, Git, and side panel behavior is sensible in the constrained layout

### Phase 5: build, release, and operationalization

The release path still needs dedicated work:

- add a reproducible iOS build and packaging workflow outside of the Xcode prebuild step
- set up code signing and provisioning
- add CI or documented release steps for iOS artifacts
- prepare App Store Connect and TestFlight distribution
- write end-user and contributor documentation for building and testing the app

## Build Prerequisites

Before running `cargo check -p zed_ios --target aarch64-apple-ios-sim` or building the Xcode project:

- make sure `xcode-select` points at a full Xcode installation
- make sure the Apple Metal compiler is available to `xcrun`
- use `script/setup-ios-toolchain` to validate the setup
- if the Metal compiler is missing on macOS 26 / Xcode 26, run `script/setup-ios-toolchain --install-metal-toolchain`

The current simulator/device build failure that mentions a missing Metal toolchain is an environment issue, not a signal that the in-tree iOS port should be replaced.

## Known Gaps and Risks

- A full local `cargo check -p zed_ios` may still depend on Apple tooling being installed correctly, including the Metal toolchain.
- The iOS renderer still contains at least one `unimplemented!()` fallback in `metal_atlas.rs`, so renderer edge cases need continued attention.
- The app is intentionally remote-only today. Local workspace or document-picker support is out of scope for the current design.
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
