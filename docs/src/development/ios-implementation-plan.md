# Zed for iPadOS - Implementation Plan

> **Status:** Phase 1 Complete - GPUI compiles for iOS  
> **Target:** iOS 26+  
> **Approach:** Remote-first thin client  
> **Primary Input:** Hardware keyboard

## Overview

This document outlines the implementation plan for bringing Zed to iPadOS as a remote-first code editor. The iPad client connects to a remote machine running `zed-remote-server`, leveraging Zed's existing remote development infrastructure.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    iPad Client                          │
│  ┌─────────────────────────────────────────────────┐   │
│  │  GPUI (Metal renderer - shared with macOS)      │   │
│  ├─────────────────────────────────────────────────┤   │
│  │  iOS Platform Layer (minimal UIKit)             │   │
│  ├─────────────────────────────────────────────────┤   │
│  │  Remote Client (existing crates/remote/)        │   │
│  ├─────────────────────────────────────────────────┤   │
│  │  Setup Tutorial UI                              │   │
│  └─────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
                          │
                          │ SSH tunnel (existing protocol)
                          ▼
┌─────────────────────────────────────────────────────────┐
│         Remote Server (user's dev machine)              │
│         zed-remote-server (already exists)              │
└─────────────────────────────────────────────────────────┘
```

### Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Connection method | SSH tunnel | No changes to external code; uses existing infrastructure |
| Offline editing | Remote only | Simplifies implementation; avoids iOS sandboxing issues |
| iOS version | iOS 26+ | Latest APIs; all target devices are powerful |
| Primary input | Hardware keyboard | Minimizes touch UI work; matches developer workflow |
| Code reuse | Share macOS Metal/CoreText code | ~70% platform code shared |

---

## Progress Tracker

### Phase 1: iOS Platform Layer for GPUI ✅ COMPLETE
- [x] 1.1 Create platform module structure
- [x] 1.2 Implement `IosPlatform` (Platform trait)
- [x] 1.3 Implement `IosWindow` (Metal rendering)
- [x] 1.4 Implement input handling (keyboard + touch)
- [x] 1.5 Update Cargo.toml with iOS dependencies
- [x] 1.6 Add conditional compilation to platform.rs
- [x] **Milestone: GPUI compiles for iOS target**

### Phase 2: iOS App Crate
- [ ] 2.1 Create `crates/zed_ios` crate structure
- [ ] 2.2 Implement app entry point
- [ ] 2.3 Set up Xcode project
- [ ] **Milestone: App launches and shows GPUI content**

### Phase 3: Connection & Tutorial UI
- [ ] 3.1 Implement `ConnectView` (connection UI)
- [ ] 3.2 Wire up remote connection logic
- [ ] 3.3 Create tutorial content
- [ ] 3.4 Implement `TutorialView`
- [ ] 3.5 Implement connection storage (Keychain)
- [ ] **Milestone: Can connect to remote server**

### Phase 4: Feature Adaptations
- [ ] 4.1 Add feature gates for iOS-unavailable features
- [ ] 4.2 Hide/disable local-only UI elements
- [ ] 4.3 Test full editor workflow over remote
- [ ] 4.4 UI adjustments (safe area, etc.)
- [ ] **Milestone: Can edit remote files end-to-end**

### Phase 5: Build & Distribution
- [ ] 5.1 Create `script/bundle-ios` build script
- [ ] 5.2 Finalize Xcode project configuration
- [ ] 5.3 Set up code signing
- [ ] 5.4 TestFlight distribution
- [ ] 5.5 Documentation
- [ ] **Milestone: App available on TestFlight**

---

## Phase 1: iOS Platform Layer for GPUI

### 1.1 Create Platform Module Structure

**New files to create:**

```
crates/gpui/src/platform/ios/
├── mod.rs                 # Module exports
├── platform.rs            # IosPlatform implementing Platform trait
├── window.rs              # UIWindow + CAMetalLayer + input handling
├── display.rs             # UIScreen wrapper
├── dispatcher.rs          # GCD dispatcher (adapt from mac)
└── app_delegate.rs        # UIApplicationDelegate via objc
```

**Code shared with macOS (no changes needed):**
- `mac/metal_renderer.rs` - Works on iOS directly
- `mac/metal_atlas.rs` - Works on iOS directly  
- `mac/text_system.rs` - Core Text API identical on iOS

### 1.2 Implement IosPlatform

The `Platform` trait has ~40 methods. Implementation strategy:

| Method | iOS Implementation |
|--------|-------------------|
| `run()` | `UIApplicationMain` via objc |
| `quit()` | No-op (iOS manages lifecycle) |
| `restart()` | No-op |
| `activate()` | No-op |
| `hide()` / `hide_other_apps()` | No-op |
| `unhide_other_apps()` | No-op |
| `displays()` | Return `[UIScreen.main]` |
| `primary_display()` | `UIScreen.main` |
| `active_window()` | Return current window handle |
| `open_window()` | Create `UIWindow` with metal-backed `UIView` |
| `window_appearance()` | Query `UITraitCollection.userInterfaceStyle` |
| `open_url()` | `UIApplication.shared.open()` |
| `on_open_urls()` | Store callback for URL handling |
| `register_url_scheme()` | No-op (configured in Info.plist) |
| `prompt_for_paths()` | Return error (remote-only) |
| `prompt_for_new_path()` | Return error (remote-only) |
| `reveal_path()` | No-op |
| `on_quit()` | Store callback |
| `on_reopen()` | No-op |
| `set_menus()` | No-op (no menu bar) |
| `set_dock_menu()` | No-op |
| `on_app_menu_action()` | No-op |
| `on_will_open_app_menu()` | No-op |
| `on_validate_app_menu_command()` | No-op |
| `set_cursor_style()` | No-op (no cursor) |
| `should_auto_hide_scrollbars()` | Return `true` |
| `write_to_clipboard()` | `UIPasteboard.general` |
| `read_from_clipboard()` | `UIPasteboard.general` |
| `write_credentials()` | iOS Keychain |
| `read_credentials()` | iOS Keychain |
| `delete_credentials()` | iOS Keychain |
| `background_executor()` | GCD-based |
| `foreground_executor()` | GCD-based |
| `text_system()` | Core Text (reuse mac) |

### 1.3 Implement IosWindow

```rust
pub struct IosWindow {
    window: id,                    // UIWindow
    view: id,                      // Custom UIView subclass  
    view_controller: id,           // UIViewController
    renderer: MetalRenderer,       // Reuse from mac/metal_renderer.rs
    input_handler: Option<PlatformInputHandler>,
    scale_factor: f32,
    
    // Callbacks
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    // ... other callbacks
}
```

**UIView subclass requirements:**
- Override `layerClass` to return `CAMetalLayer`
- Implement `touchesBegan`, `touchesMoved`, `touchesEnded`
- Add gesture recognizers for pan (scroll) and long-press (context menu)

**Key `PlatformWindow` trait methods:**

| Method | Implementation |
|--------|----------------|
| `bounds()` | `view.bounds` converted to Pixels |
| `content_size()` | `view.bounds.size` minus safe area |
| `scale_factor()` | `UIScreen.main.scale` |
| `appearance()` | Map `UIUserInterfaceStyle` to `WindowAppearance` |
| `draw()` | `self.renderer.draw(scene)` (same as macOS) |
| `sprite_atlas()` | From MetalRenderer |
| `set_input_handler()` | Store handler, wire to UIKeyInput |
| `on_input()` | Store callback |
| `on_resize()` | Store callback, observe bounds changes |

### 1.4 Implement Input Handling

**Hardware keyboard (primary):**

```rust
// Map UIKit key events to GPUI
fn handle_key_press(press: UIPress) -> Option<PlatformInput> {
    let key = press.key()?;
    let keystroke = Keystroke {
        key: map_ui_key_to_string(key.keyCode()),
        modifiers: Modifiers {
            control: key.modifierFlags().contains(.control),
            alt: key.modifierFlags().contains(.alternate),
            shift: key.modifierFlags().contains(.shift),
            command: key.modifierFlags().contains(.command),
            function: key.modifierFlags().contains(.numericPad),
        },
        ime_key: None,
    };
    Some(PlatformInput::KeyDown(KeyDownEvent {
        keystroke,
        is_held: press.isRepeating(),
    }))
}
```

**Touch input (secondary):**

```rust
fn translate_touch(touch: UITouch, view: UIView) -> PlatformInput {
    let location = touch.location_in_view(view);
    let position = point(px(location.x as f32), px(location.y as f32));
    
    match touch.phase() {
        UITouchPhase::Began => PlatformInput::MouseDown(MouseDownEvent {
            position,
            button: MouseButton::Left,
            click_count: touch.tapCount() as usize,
            modifiers: Modifiers::default(),
        }),
        UITouchPhase::Moved => PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
        UITouchPhase::Ended | UITouchPhase::Cancelled => {
            PlatformInput::MouseUp(MouseUpEvent {
                position,
                button: MouseButton::Left,
                click_count: touch.tapCount() as usize,
                modifiers: Modifiers::default(),
            })
        }
    }
}
```

**Scroll gesture:**

```rust
fn handle_pan_gesture(gesture: UIPanGestureRecognizer, view: UIView) -> PlatformInput {
    let translation = gesture.translation_in_view(view);
    let location = gesture.location_in_view(view);
    
    PlatformInput::ScrollWheel(ScrollWheelEvent {
        position: point(px(location.x as f32), px(location.y as f32)),
        delta: ScrollDelta::Pixels(point(
            px(translation.x as f32),
            px(translation.y as f32),
        )),
        modifiers: Modifiers::default(),
        touch_phase: map_gesture_state(gesture.state()),
    })
}
```

### 1.5 Cargo.toml Changes

```toml
# crates/gpui/Cargo.toml additions

[target.'cfg(target_os = "ios")'.dependencies]
block = "0.1"
core-foundation.workspace = true
core-foundation-sys.workspace = true
core-graphics = "0.24"
core-text = "21"
metal.workspace = true
objc.workspace = true

# Move these to shared Apple target
[target.'cfg(any(target_os = "macos", target_os = "ios"))'.dependencies]
pathfinder_geometry = "0.5"
```

### 1.6 Conditional Compilation

```rust
// crates/gpui/src/platform.rs - additions

#[cfg(target_os = "ios")]
mod ios;

#[cfg(target_os = "ios")]
pub(crate) use ios::*;

#[cfg(target_os = "ios")]
pub(crate) fn current_platform(_headless: bool) -> Rc<dyn Platform> {
    Rc::new(ios::IosPlatform::new())
}
```

---

## Phase 2: iOS App Crate

### 2.1 Crate Structure

```
crates/zed_ios/
├── Cargo.toml
├── src/
│   ├── lib.rs               # Library root
│   ├── main.rs              # iOS entry point (calls lib)
│   ├── app.rs               # App initialization
│   ├── connect_view.rs      # Remote connection UI
│   └── tutorial_view.rs     # Setup tutorial
├── resources/
│   └── tutorial/
│       ├── 01-install.md
│       ├── 02-start-server.md
│       ├── 03-ssh-setup.md
│       └── 04-connect.md
└── ios/
    ├── Zed.xcodeproj/
    ├── Zed/
    │   ├── main.m
    │   ├── Info.plist
    │   ├── Entitlements.plist
    │   └── Assets.xcassets/
    └── libzed_ios.a         # Built by Cargo
```

### 2.2 Cargo.toml

```toml
[package]
name = "zed_ios"
version = "0.1.0"
edition.workspace = true
publish.workspace = true

[lib]
crate-type = ["staticlib", "lib"]

[dependencies]
gpui.workspace = true
remote.workspace = true
client.workspace = true
workspace.workspace = true
editor.workspace = true
project.workspace = true
theme.workspace = true
settings.workspace = true
assets.workspace = true
ui.workspace = true
markdown.workspace = true
util.workspace = true
anyhow.workspace = true
futures.workspace = true
serde.workspace = true
serde_json.workspace = true
log.workspace = true
```

### 2.3 Entry Point

```rust
// src/main.rs
use gpui::App;
use zed_ios::ZedIosApp;

fn main() {
    env_logger::init();
    
    App::new().run(|cx| {
        ZedIosApp::init(cx);
    });
}

// src/lib.rs
mod app;
mod connect_view;
mod tutorial_view;

pub use app::ZedIosApp;
```

```rust
// src/app.rs
use gpui::{App, Context, Window, WindowOptions};
use crate::connect_view::ConnectView;

pub struct ZedIosApp;

impl ZedIosApp {
    pub fn init(cx: &mut App) {
        // Load assets
        assets::init(cx);
        
        // Load themes
        theme::init(cx);
        
        // Load settings
        settings::init(cx);
        
        // Open main window with connection UI
        cx.open_window(
            WindowOptions {
                titlebar: None,  // iOS doesn't have titlebar
                ..Default::default()
            },
            |_, cx| cx.new(|cx| ConnectView::new(cx)),
        )
        .expect("Failed to open window");
    }
}
```

### 2.4 Xcode Project

**Info.plist:**
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>Zed</string>
    <key>CFBundleIdentifier</key>
    <string>dev.zed.Zed</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>CFBundleShortVersionString</key>
    <string>1.0.0</string>
    <key>UIRequiredDeviceCapabilities</key>
    <array>
        <string>arm64</string>
    </array>
    <key>UISupportedInterfaceOrientations</key>
    <array>
        <string>UIInterfaceOrientationPortrait</string>
        <string>UIInterfaceOrientationLandscapeLeft</string>
        <string>UIInterfaceOrientationLandscapeRight</string>
        <string>UIInterfaceOrientationPortraitUpsideDown</string>
    </array>
    <key>UILaunchScreen</key>
    <dict/>
    <key>UIApplicationSupportsIndirectInputEvents</key>
    <true/>
</dict>
</plist>
```

**Entitlements.plist:**
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>keychain-access-groups</key>
    <array>
        <string>$(AppIdentifierPrefix)dev.zed.Zed</string>
    </array>
</dict>
</plist>
```

**main.m:**
```objc
#import <UIKit/UIKit.h>

// Defined in Rust
extern void rust_main(void);

int main(int argc, char * argv[]) {
    @autoreleasepool {
        rust_main();
    }
    return 0;
}
```

---

## Phase 3: Connection & Tutorial UI

### 3.1 ConnectView

```rust
// src/connect_view.rs
use gpui::*;
use ui::*;

pub struct ConnectView {
    hostname: String,
    username: String,
    auth_method: AuthMethod,
    recent_connections: Vec<SavedConnection>,
    connection_state: ConnectionState,
}

#[derive(Clone)]
enum AuthMethod {
    SshKey,
    Password,
}

enum ConnectionState {
    Idle,
    Connecting,
    Connected,
    Error(String),
}

struct SavedConnection {
    hostname: String,
    username: String,
}

impl ConnectView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            hostname: String::new(),
            username: String::new(),
            auth_method: AuthMethod::SshKey,
            recent_connections: Self::load_recent_connections(cx),
            connection_state: ConnectionState::Idle,
        }
    }
    
    fn connect(&mut self, cx: &mut Context<Self>) {
        self.connection_state = ConnectionState::Connecting;
        cx.notify();
        
        let hostname = self.hostname.clone();
        let username = self.username.clone();
        
        cx.spawn(async move |this, mut cx| {
            // Use existing remote connection infrastructure
            match remote::connect_ssh(&hostname, &username).await {
                Ok(connection) => {
                    this.update(&mut cx, |this, cx| {
                        this.connection_state = ConnectionState::Connected;
                        // Transition to workspace
                        Self::open_remote_workspace(connection, cx);
                    })?;
                }
                Err(e) => {
                    this.update(&mut cx, |this, cx| {
                        this.connection_state = ConnectionState::Error(e.to_string());
                        cx.notify();
                    })?;
                }
            }
            Ok(())
        })
        .detach_and_log_err(cx);
    }
    
    fn show_tutorial(&mut self, cx: &mut Context<Self>) {
        // Open tutorial view
    }
    
    fn load_recent_connections(cx: &App) -> Vec<SavedConnection> {
        // Load from Keychain
        vec![]
    }
}

impl Render for ConnectView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .justify_center()
            .items_center()
            .gap_4()
            .child(
                div()
                    .text_xl()
                    .child("Welcome to Zed for iPad")
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .rounded_lg()
                    .bg(cx.theme().colors().surface)
                    .child(self.render_connection_form(cx))
            )
            .child(
                Button::new("tutorial", "View Setup Tutorial")
                    .on_click(cx.listener(|this, _, _, cx| this.show_tutorial(cx)))
            )
            .when(!self.recent_connections.is_empty(), |this| {
                this.child(self.render_recent_connections(cx))
            })
    }
}
```

### 3.2 Tutorial Content

**resources/tutorial/01-install.md:**
```markdown
# Step 1: Install Zed on Your Remote Machine

First, install Zed on the computer you want to connect to.

## macOS

Open Terminal and run:

```bash
brew install zed
```

Or download from [zed.dev](https://zed.dev)

## Linux

```bash
curl -f https://zed.dev/install.sh | sh
```

## Windows

Download the installer from [zed.dev](https://zed.dev)

---

Once installed, continue to the next step.
```

**resources/tutorial/02-start-server.md:**
```markdown
# Step 2: Start the Remote Server

On your remote machine, start the Zed remote server:

```bash
zed --remote-server
```

The server will start listening for connections.

> **Tip:** You can add this to your shell profile to start automatically,
> or set up a systemd service on Linux.

---

Continue to set up SSH access.
```

**resources/tutorial/03-ssh-setup.md:**
```markdown
# Step 3: Ensure SSH Access

Zed for iPad connects via SSH. Make sure SSH is enabled on your remote machine.

## Verify SSH is Running

**macOS:** System Settings → General → Sharing → Remote Login

**Linux:** 
```bash
sudo systemctl status sshd
```

**Windows:** Enable OpenSSH Server in Settings → Apps → Optional Features

## Test Your Connection

From another device, verify you can connect:

```bash
ssh your-username@your-hostname
```

## (Recommended) Set Up SSH Keys

For passwordless authentication, copy your SSH key to the remote machine:

```bash
ssh-copy-id your-username@your-hostname
```

---

You're ready to connect from your iPad!
```

### 3.3 TutorialView

```rust
// src/tutorial_view.rs
use gpui::*;
use markdown::Markdown;

pub struct TutorialView {
    current_step: usize,
    steps: Vec<TutorialStep>,
}

struct TutorialStep {
    title: String,
    content: String,  // Markdown
}

impl TutorialView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            current_step: 0,
            steps: Self::load_steps(),
        }
    }
    
    fn load_steps() -> Vec<TutorialStep> {
        vec![
            TutorialStep {
                title: "Install Zed".into(),
                content: include_str!("../resources/tutorial/01-install.md").into(),
            },
            TutorialStep {
                title: "Start Server".into(),
                content: include_str!("../resources/tutorial/02-start-server.md").into(),
            },
            TutorialStep {
                title: "SSH Setup".into(),
                content: include_str!("../resources/tutorial/03-ssh-setup.md").into(),
            },
            TutorialStep {
                title: "Connect".into(),
                content: include_str!("../resources/tutorial/04-connect.md").into(),
            },
        ]
    }
    
    fn next_step(&mut self, cx: &mut Context<Self>) {
        if self.current_step < self.steps.len() - 1 {
            self.current_step += 1;
            cx.notify();
        }
    }
    
    fn prev_step(&mut self, cx: &mut Context<Self>) {
        if self.current_step > 0 {
            self.current_step -= 1;
            cx.notify();
        }
    }
}

impl Render for TutorialView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let step = &self.steps[self.current_step];
        
        div()
            .flex()
            .flex_col()
            .size_full()
            .child(
                // Step indicator
                div()
                    .flex()
                    .gap_2()
                    .p_4()
                    .children(
                        (0..self.steps.len()).map(|i| {
                            div()
                                .w_3()
                                .h_3()
                                .rounded_full()
                                .when(i == self.current_step, |d| d.bg(cx.theme().colors().accent))
                                .when(i != self.current_step, |d| d.bg(cx.theme().colors().border))
                        })
                    )
            )
            .child(
                // Content
                div()
                    .flex_1()
                    .p_4()
                    .overflow_y_scroll()
                    .child(Markdown::new(step.content.clone()))
            )
            .child(
                // Navigation
                div()
                    .flex()
                    .justify_between()
                    .p_4()
                    .child(
                        Button::new("prev", "Previous")
                            .disabled(self.current_step == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.prev_step(cx)))
                    )
                    .child(
                        Button::new("next", "Next")
                            .disabled(self.current_step == self.steps.len() - 1)
                            .on_click(cx.listener(|this, _, _, cx| this.next_step(cx)))
                    )
            )
    }
}
```

---

## Phase 4: Feature Adaptations

### 4.1 Feature Gates

Create utility functions to check platform capabilities:

```rust
// Could be in crates/util/src/platform.rs or similar

/// Returns true if local terminal is available
pub fn supports_local_terminal() -> bool {
    cfg!(not(target_os = "ios"))
}

/// Returns true if local LSP servers can be spawned
pub fn supports_local_lsp() -> bool {
    cfg!(not(target_os = "ios"))
}

/// Returns true if local file system access is available
pub fn supports_local_filesystem() -> bool {
    cfg!(not(target_os = "ios"))
}

/// Returns true if the platform has a menu bar
pub fn has_menu_bar() -> bool {
    cfg!(not(target_os = "ios"))
}

/// Returns true if cursor styles are supported
pub fn supports_cursor_styles() -> bool {
    cfg!(not(target_os = "ios"))
}
```

### 4.2 UI Element Visibility

Modify UI components to hide unavailable features:

```rust
// Example: In workspace toolbar
fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        // Only show terminal button if supported
        .when(util::supports_local_terminal(), |this| {
            this.child(
                IconButton::new("terminal", Icon::Terminal)
                    .on_click(cx.listener(|_, _, _, cx| /* open terminal */))
            )
        })
        // ... other buttons
}
```

### 4.3 Features Disabled on iOS

| Feature | Modification |
|---------|--------------|
| Local terminal | Hide "New Terminal" action and menu items |
| Local file open | Hide "Open File" (only "Open Remote") |
| Auto-update | Completely disabled (App Store handles) |
| CLI installation | Hide "Install CLI" menu item |
| Multiple windows | Disabled (future enhancement) |
| Cursor styles | `set_cursor_style()` is no-op |
| Dock menu | `set_dock_menu()` is no-op |
| App menu | `set_menus()` is no-op |

---

## Phase 5: Build & Distribution

### 5.1 Build Script

**script/bundle-ios:**
```bash
#!/bin/bash
set -euo pipefail

# Configuration
SCHEME="Zed"
CONFIGURATION="${1:-Release}"
RUST_TARGET="aarch64-apple-ios"
IOS_DEPLOYMENT_TARGET="26.0"

echo "Building Zed for iOS ($CONFIGURATION)"

# Step 1: Build Rust static library
echo "Building Rust library..."
export IPHONEOS_DEPLOYMENT_TARGET="$IOS_DEPLOYMENT_TARGET"

if [ "$CONFIGURATION" == "Release" ]; then
    cargo build --release --target "$RUST_TARGET" -p zed_ios
    RUST_LIB="target/$RUST_TARGET/release/libzed_ios.a"
else
    cargo build --target "$RUST_TARGET" -p zed_ios
    RUST_LIB="target/$RUST_TARGET/debug/libzed_ios.a"
fi

# Step 2: Copy library to Xcode project
mkdir -p "crates/zed_ios/ios/Frameworks"
cp "$RUST_LIB" "crates/zed_ios/ios/Frameworks/"

# Step 3: Build with Xcode
echo "Building iOS app..."
xcodebuild \
    -project "crates/zed_ios/ios/Zed.xcodeproj" \
    -scheme "$SCHEME" \
    -configuration "$CONFIGURATION" \
    -destination 'generic/platform=iOS' \
    -derivedDataPath "target/ios-build" \
    build

echo "Build complete!"
echo "App bundle: target/ios-build/Build/Products/$CONFIGURATION-iphoneos/Zed.app"
```

### 5.2 CI/CD Configuration

**.github/workflows/build-ios.yml:**
```yaml
name: Build iOS

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  build:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
        with:
          targets: aarch64-apple-ios
      
      - name: Build iOS
        run: ./script/bundle-ios
      
      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: Zed-iOS
          path: target/ios-build/Build/Products/Release-iphoneos/Zed.app
```

### 5.3 TestFlight Distribution

1. Create App Store Connect record for "Zed" iOS app
2. Generate distribution certificate and provisioning profile
3. Archive and upload via Xcode or `xcrun altool`
4. Configure TestFlight testers

---

## File Summary

### New Files to Create

| Path | Purpose | Est. Lines |
|------|---------|------------|
| `crates/gpui/src/platform/ios/mod.rs` | Module exports | ~50 |
| `crates/gpui/src/platform/ios/platform.rs` | Platform trait impl | ~400 |
| `crates/gpui/src/platform/ios/window.rs` | UIWindow/UIView/Metal | ~600 |
| `crates/gpui/src/platform/ios/display.rs` | UIScreen wrapper | ~100 |
| `crates/gpui/src/platform/ios/dispatcher.rs` | GCD dispatcher | ~200 |
| `crates/gpui/src/platform/ios/app_delegate.rs` | UIApplicationDelegate | ~150 |
| `crates/zed_ios/Cargo.toml` | Crate manifest | ~50 |
| `crates/zed_ios/src/lib.rs` | Library root | ~20 |
| `crates/zed_ios/src/main.rs` | Entry point | ~20 |
| `crates/zed_ios/src/app.rs` | App initialization | ~100 |
| `crates/zed_ios/src/connect_view.rs` | Connection UI | ~250 |
| `crates/zed_ios/src/tutorial_view.rs` | Tutorial UI | ~150 |
| `crates/zed_ios/resources/tutorial/*.md` | Tutorial content | ~400 |
| `crates/zed_ios/ios/Zed.xcodeproj/` | Xcode project | N/A |
| `crates/zed_ios/ios/Zed/main.m` | ObjC entry | ~15 |
| `crates/zed_ios/ios/Zed/Info.plist` | App config | ~50 |
| `crates/zed_ios/ios/Zed/Entitlements.plist` | Entitlements | ~15 |
| `script/bundle-ios` | Build script | ~50 |

**Total new Rust code:** ~2,100 lines

### Files to Modify

| Path | Change |
|------|--------|
| `crates/gpui/src/platform.rs` | Add iOS module, `current_platform()` for iOS |
| `crates/gpui/Cargo.toml` | Add iOS dependencies |
| `Cargo.toml` (workspace) | Add `zed_ios` to members |

---

## Timeline Estimate

| Phase | Duration | Cumulative |
|-------|----------|------------|
| Phase 1: GPUI Platform Layer | 3-4 weeks | 3-4 weeks |
| Phase 2: iOS App Crate | 1-2 weeks | 4-6 weeks |
| Phase 3: Connection & Tutorial | 2-3 weeks | 6-9 weeks |
| Phase 4: Feature Adaptations | 1-2 weeks | 7-11 weeks |
| Phase 5: Build & Distribution | 1-2 weeks | 8-13 weeks |

**Total: 8-13 weeks** depending on complexity encountered.

---

## Open Questions / Decisions Needed

1. **App identifier:** `dev.zed.Zed` or `dev.zed.Zed-iOS`?
2. **TestFlight group:** Who should be in initial beta?
3. **Branding:** Same icon/name as desktop, or distinct "Zed for iPad"?
4. **Pricing:** Free, paid, or subscription?

---

## References

- GPUI platform abstraction: `crates/gpui/src/platform/`
- macOS platform implementation: `crates/gpui/src/platform/mac/`
- Remote development: `crates/remote/`, `crates/remote_server/`
- Existing Metal renderer: `crates/gpui/src/platform/mac/metal_renderer.rs`
