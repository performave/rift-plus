// laggyapp -- a window that is slow to apply what it is told, the way Electron is.
//
// The resize bugs that matter only appear against an app that cannot keep up.
// TextEdit and Safari in the guest apply a frame within a millisecond, so a
// harness built on them reproduces nothing and every reading from it is about
// the harness. This blocks its main thread in bursts, which is what a heavy
// renderer does between paints: accessibility writes queue behind it and are
// applied late and in arrears, which is the condition under test.
//
//   clang -fobjc-arc -framework Cocoa -o laggyapp laggyapp.m
//   ./laggyapp <stall-ms> <every-ms>
#import <Cocoa/Cocoa.h>

@interface Lag : NSObject <NSApplicationDelegate>
@property(nonatomic) NSWindow *win;
@property(nonatomic) int stallMs;
@property(nonatomic) int everyMs;
@end

@implementation Lag
- (void)applicationDidFinishLaunching:(NSNotification *)n {
    NSRect r = NSMakeRect(200, 200, 700, 500);
    self.win = [[NSWindow alloc]
        initWithContentRect:r
                  styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskResizable |
                            NSWindowStyleMaskClosable
                    backing:NSBackingStoreBuffered
                      defer:NO];
    [self.win setTitle:[NSString stringWithFormat:@"laggy %dms/%dms",
                                                  self.stallMs, self.everyMs]];
    // A minimum of its own, like the apps that have one.
    [self.win setContentMinSize:NSMakeSize(300, 200)];
    [self.win makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];

    // The stall. On the main thread, so it blocks the same run loop that
    // services accessibility writes -- a background thread would not.
    [NSTimer scheduledTimerWithTimeInterval:(self.everyMs / 1000.0)
                                    repeats:YES
                                      block:^(NSTimer *t) {
                                        usleep(self.stallMs * 1000);
                                      }];
}
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)s { return YES; }
@end

int main(int argc, const char **argv) {
    @autoreleasepool {
        Lag *lag = [Lag new];
        lag.stallMs = argc > 1 ? atoi(argv[1]) : 120;
        lag.everyMs = argc > 2 ? atoi(argv[2]) : 60;
        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyRegular];
        [app setDelegate:lag];
        [app run];
    }
    return 0;
}
