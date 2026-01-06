// Zed for iPadOS - Application Entry Point
//
// This file contains the Objective-C entry point for the iOS application.
// It sets up the iOS run loop via UIApplicationMain and initializes the
// Rust-based Zed application when the app finishes launching.

#import <UIKit/UIKit.h>

// Rust entry point - defined in zed_ios crate
extern void zed_ios_init(void);

@interface ZedAppDelegate : UIResponder <UIApplicationDelegate>
@end

@implementation ZedAppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)launchOptions {
    NSLog(@"[Zed] didFinishLaunchingWithOptions - calling zed_ios_init");
    
    // Initialize the Rust application
    // This creates the GPUI App, opens the main window, and starts rendering.
    // The iOS run loop is already running at this point (started by UIApplicationMain).
    zed_ios_init();
    
    NSLog(@"[Zed] zed_ios_init returned");
    return YES;
}

- (void)applicationWillResignActive:(UIApplication *)application {
    NSLog(@"[Zed] applicationWillResignActive");
}

- (void)applicationDidEnterBackground:(UIApplication *)application {
    NSLog(@"[Zed] applicationDidEnterBackground");
}

- (void)applicationWillEnterForeground:(UIApplication *)application {
    NSLog(@"[Zed] applicationWillEnterForeground");
}

- (void)applicationDidBecomeActive:(UIApplication *)application {
    NSLog(@"[Zed] applicationDidBecomeActive");
}

@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        NSLog(@"[Zed] main() starting UIApplicationMain");
        // UIApplicationMain starts the iOS run loop and never returns.
        // It creates the application object, sets up the event loop,
        // and calls the app delegate methods at appropriate times.
        return UIApplicationMain(argc, argv, nil, NSStringFromClass([ZedAppDelegate class]));
    }
}
