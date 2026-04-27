// Zed for iPadOS - Application Entry Point
//
// This file contains the Objective-C entry point for the iOS application.
// It sets up the iOS run loop via UIApplicationMain and initializes the
// Rust-based Zed application when the app finishes launching.

#import <UIKit/UIKit.h>

// Rust entry point - defined in zed_ios crate
extern void zed_ios_init(void);
extern int zed_ios_maybe_run_process_mode(int argc, const char *const argv[]);
extern void gpui_ios_initialize(void);
extern void gpui_ios_did_become_active(void);
extern void gpui_ios_will_resign_active(void);
extern void gpui_ios_did_enter_background(void);
extern void gpui_ios_will_enter_foreground(void);
extern void gpui_ios_will_terminate(void);
extern void gpui_ios_open_url(const char *url);

@interface ZedAppDelegate : UIResponder <UIApplicationDelegate>
@end

@implementation ZedAppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    NSLog(@"[Zed] didFinishLaunchingWithOptions - calling zed_ios_init");

    gpui_ios_initialize();

    // Initialize the Rust application
    // This creates the GPUI App, opens the main window, and starts rendering.
    // The iOS run loop is already running at this point (started by UIApplicationMain).
    zed_ios_init();

    NSLog(@"[Zed] zed_ios_init returned");
    return YES;
}

- (void)applicationWillResignActive:(UIApplication *)application {
    NSLog(@"[Zed] applicationWillResignActive");
    gpui_ios_will_resign_active();
}

- (void)applicationDidEnterBackground:(UIApplication *)application {
    NSLog(@"[Zed] applicationDidEnterBackground");
    gpui_ios_did_enter_background();
}

- (void)applicationWillEnterForeground:(UIApplication *)application {
    NSLog(@"[Zed] applicationWillEnterForeground");
    gpui_ios_will_enter_foreground();
}

- (void)applicationDidBecomeActive:(UIApplication *)application {
    NSLog(@"[Zed] applicationDidBecomeActive");
    gpui_ios_did_become_active();
}

- (void)applicationWillTerminate:(UIApplication *)application {
    NSLog(@"[Zed] applicationWillTerminate");
    gpui_ios_will_terminate();
}

- (BOOL)application:(UIApplication *)application
            openURL:(NSURL *)url
            options:(NSDictionary *)options {
    const char *utf8 = [[url absoluteString] UTF8String];
    if (utf8 != NULL) {
        gpui_ios_open_url(utf8);
    }
    return YES;
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        int process_mode_exit_code =
            zed_ios_maybe_run_process_mode(argc, (const char *const *)argv);
        if (process_mode_exit_code != -1) {
            return process_mode_exit_code;
        }

        NSLog(@"[Zed] main() starting UIApplicationMain");
        // UIApplicationMain starts the iOS run loop and never returns.
        // It creates the application object, sets up the event loop,
        // and calls the app delegate methods at appropriate times.
        return UIApplicationMain(argc, argv, nil, NSStringFromClass([ZedAppDelegate class]));
    }
}
