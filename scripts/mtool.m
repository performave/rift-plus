// mtool -- synthesize pointer input in the guest, so a scenario can drive rift
// the way a hand does rather than only through the CLI.
//
// Apple Events are the obvious route and hang indefinitely inside a LaunchAgent
// here (see docs/vm-harness-handoff.md), so this posts CGEvents directly, the
// same way `dtool fullscreen` posts its key. A drag is sent as a down, a run of
// dragged events and an up: rift's tap reads movement, not teleports, and a
// single jump from start to finish is not a gesture it will follow.
//
//   clang -fobjc-arc -framework CoreGraphics -framework Foundation -o mtool mtool.m
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>

static void post(CGEventType type, CGPoint at, CGMouseButton button, CGEventFlags flags) {
    CGEventRef e = CGEventCreateMouseEvent(NULL, type, at, button);
    if (!e) return;
    if (flags) CGEventSetFlags(e, flags);
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
}

static CGEventFlags parse_flags(const char *name) {
    if (!name) return 0;
    CGEventFlags f = 0;
    if (strstr(name, "cmd")) f |= kCGEventFlagMaskCommand;
    if (strstr(name, "alt") || strstr(name, "opt")) f |= kCGEventFlagMaskAlternate;
    if (strstr(name, "ctrl")) f |= kCGEventFlagMaskControl;
    if (strstr(name, "shift")) f |= kCGEventFlagMaskShift;
    return f;
}

int main(int argc, const char *argv[]) { @autoreleasepool {
    const char *cmd = argc > 1 ? argv[1] : "";

    if (!strcmp(cmd, "move") && argc > 3) {
        post(kCGEventMouseMoved, CGPointMake(atof(argv[2]), atof(argv[3])), 0, 0);
        printf("moved to %s,%s\n", argv[2], argv[3]);
        return 0;
    }

    if (!strcmp(cmd, "click") && argc > 3) {
        CGPoint p = CGPointMake(atof(argv[2]), atof(argv[3]));
        post(kCGEventMouseMoved, p, 0, 0);
        usleep(40000);
        post(kCGEventLeftMouseDown, p, kCGMouseButtonLeft, 0);
        usleep(40000);
        post(kCGEventLeftMouseUp, p, kCGMouseButtonLeft, 0);
        printf("clicked %s,%s\n", argv[2], argv[3]);
        return 0;
    }

    // drag x1 y1 x2 y2 [modifiers] [steps]
    if (!strcmp(cmd, "drag") && argc > 5) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        CGPoint b = CGPointMake(atof(argv[4]), atof(argv[5]));
        CGEventFlags flags = argc > 6 ? parse_flags(argv[6]) : 0;
        int steps = argc > 7 ? atoi(argv[7]) : 24;
        if (steps < 2) steps = 2;

        post(kCGEventMouseMoved, a, 0, flags);
        usleep(80000);
        post(kCGEventLeftMouseDown, a, kCGMouseButtonLeft, flags);
        usleep(80000);
        for (int i = 1; i <= steps; i++) {
            double t = (double)i / steps;
            CGPoint p = CGPointMake(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
            post(kCGEventLeftMouseDragged, p, kCGMouseButtonLeft, flags);
            usleep(16000);   // ~60Hz, the rate a hand produces
        }
        usleep(80000);
        post(kCGEventLeftMouseUp, b, kCGMouseButtonLeft, flags);
        printf("dragged %.0f,%.0f -> %.0f,%.0f flags=0x%llx steps=%d\n",
               a.x, a.y, b.x, b.y, (unsigned long long)flags, steps);
        return 0;
    }

    fprintf(stderr, "usage: mtool move <x> <y> | click <x> <y> | "
                    "drag <x1> <y1> <x2> <y2> [cmd+alt+ctrl+shift] [steps]\n");
    return 1;
} }
