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

    // drag x1 y1 x2 y2 [modifiers] [steps] [left|right]
    if (!strcmp(cmd, "drag") && argc > 5) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        CGPoint b = CGPointMake(atof(argv[4]), atof(argv[5]));
        CGEventFlags flags = argc > 6 ? parse_flags(argv[6]) : 0;
        int steps = argc > 7 ? atoi(argv[7]) : 24;
        if (steps < 2) steps = 2;
        // rift gives the modifier's two buttons different jobs -- `action1` on
        // the left, `action2` on the right -- so a resize test that only ever
        // sends the left button is testing move.
        int right = argc > 8 && !strcmp(argv[8], "right");
        CGMouseButton button = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
        CGEventType downType = right ? kCGEventRightMouseDown : kCGEventLeftMouseDown;
        CGEventType moveType = right ? kCGEventRightMouseDragged : kCGEventLeftMouseDragged;
        CGEventType upType = right ? kCGEventRightMouseUp : kCGEventLeftMouseUp;

        post(kCGEventMouseMoved, a, 0, flags);
        usleep(80000);
        post(downType, a, button, flags);
        usleep(80000);
        for (int i = 1; i <= steps; i++) {
            double t = (double)i / steps;
            CGPoint p = CGPointMake(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
            post(moveType, p, button, flags);
            usleep(16000);   // ~60Hz, the rate a hand produces
        }
        usleep(80000);
        post(upType, b, button, flags);
        printf("dragged %.0f,%.0f -> %.0f,%.0f flags=0x%llx steps=%d button=%s\n",
               a.x, a.y, b.x, b.y, (unsigned long long)flags, steps,
               right ? "right" : "left");
        return 0;
    }

    // gesture dock-swipe [phase] | gesture processed [phase]
    //
    // rift reads gestures off the event tap as CGEvents of type 29 (gesture)
    // and 30 (dock control), pulling the IOHID event out of them only for a
    // *raw* contact frame. Two of its three decode paths never touch IOHID:
    // a horizontal dock swipe is fields 110 and 123, and a processed gesture
    // is a non-zero phase in field 132. Both of those are ordinary CGEvent
    // integer fields, so they can be posted from here -- which is the
    // difference between "this VM has no trackpad so gestures are untestable"
    // and "the raw touch-frame path is untestable". Only the last needs a
    // virtual HID digitizer.
    if (!strcmp(cmd, "gesture") && argc > 2) {
        const int kGestureType = 29, kDockControlType = 30;
        const int kHidTypeField = 110, kSwipeMotionField = 123, kPhaseField = 132;
        const int kDockSwipe = 23, kHorizontal = 1;
        int phase = (argc > 3) ? atoi(argv[3]) : 1;

        CGEventRef e = CGEventCreate(NULL);
        if (!e) { fprintf(stderr, "could not create the event\n"); return 2; }

        if (!strcmp(argv[2], "dock-swipe")) {
            CGEventSetType(e, (CGEventType)kDockControlType);
            CGEventSetIntegerValueField(e, (CGEventField)kHidTypeField, kDockSwipe);
            CGEventSetIntegerValueField(e, (CGEventField)kSwipeMotionField, kHorizontal);
            CGEventSetIntegerValueField(e, (CGEventField)kPhaseField, phase);
        } else if (!strcmp(argv[2], "processed")) {
            CGEventSetType(e, (CGEventType)kGestureType);
            CGEventSetIntegerValueField(e, (CGEventField)kPhaseField, phase);
        } else {
            CFRelease(e);
            fprintf(stderr, "gesture takes dock-swipe or processed\n");
            return 1;
        }
        CGEventPost(kCGHIDEventTap, e);
        printf("posted %s gesture, phase=%d\n", argv[2], phase);
        CFRelease(e);
        return 0;
    }

    fprintf(stderr, "usage: mtool move <x> <y> | click <x> <y> | "
                    "drag <x1> <y1> <x2> <y2> [cmd+alt+ctrl+shift] [steps] [left|right] | "
                    "gesture dock-swipe|processed [phase]\n");
    return 1;
} }
