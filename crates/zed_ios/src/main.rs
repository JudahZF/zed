//! Test entry point for zed_ios crate.
//!
//! This is a stub that allows `cargo build` to succeed on non-iOS platforms.
//! The actual iOS app uses zed_ios_init() called from main.m.

fn main() {
    #[cfg(target_os = "ios")]
    {
        zed_ios::zed_ios_init();
    }

    #[cfg(not(target_os = "ios"))]
    {
        eprintln!("zed_ios is an iOS-only application.");
        eprintln!("Use Xcode to build and run on iOS Simulator or device.");
        std::process::exit(1);
    }
}
