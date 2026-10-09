// hopapp -- a window that moves itself where it is told, with no pointer involved.
//
// When a desktop changes display, macOS places its windows itself a beat after
// the move, and sometimes off the far edge (float-desktop-move-test.py). The
// scripting addition's desktop move in the guest does not reproduce that
// placement, so this stands in for it: on SIGUSR1 the app sets its own window
// to the frame in /tmp/hopapp.frame -- "x y w h" in the same top-left-origin
// global coordinates rift and the window server use. To rift that is exactly a
// relocation: a frame change it did not ask for, with the button up.
//
//   clang -fobjc-arc -framework Cocoa -o hopapp hopapp.m
//   ./hopapp &           then   echo "x y w h" > /tmp/hopapp.frame; kill -USR1 <pid>

#import <Cocoa/Cocoa.h>
#include <signal.h>

static NSWindow *window;

static void hop(void) {
    NSString *spec = [NSString stringWithContentsOfFile:@"/tmp/hopapp.frame"
                                               encoding:NSUTF8StringEncoding
                                                  error:nil];
    NSArray<NSString *> *parts = [spec componentsSeparatedByCharactersInSet:
                                           [NSCharacterSet whitespaceAndNewlineCharacterSet]];
    NSMutableArray<NSNumber *> *nums = [NSMutableArray array];
    for (NSString *p in parts) {
        if (p.length) [nums addObject:@(p.doubleValue)];
    }
    if (nums.count < 4) return;
    // Cocoa's origin is the bottom-left of the primary display.
    CGFloat primary = NSScreen.screens.firstObject.frame.size.height;
    CGFloat x = nums[0].doubleValue, y = nums[1].doubleValue;
    CGFloat w = nums[2].doubleValue, h = nums[3].doubleValue;
    [window setFrame:NSMakeRect(x, primary - y - h, w, h) display:YES];
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        NSApplication *app = NSApplication.sharedApplication;
        app.activationPolicy = NSApplicationActivationPolicyRegular;
        window = [[NSWindow alloc]
            initWithContentRect:NSMakeRect(300, 300, 640, 420)
                      styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable
                                | NSWindowStyleMaskResizable | NSWindowStyleMaskMiniaturizable
                        backing:NSBackingStoreBuffered
                          defer:NO];
        window.title = @"hopapp";
        [window makeKeyAndOrderFront:nil];
        [app activateIgnoringOtherApps:YES];

        signal(SIGUSR1, SIG_IGN);
        dispatch_source_t source =
            dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, SIGUSR1, 0, dispatch_get_main_queue());
        dispatch_source_set_event_handler(source, ^{ hop(); });
        dispatch_resume(source);

        [app run];
    }
    return 0;
}
