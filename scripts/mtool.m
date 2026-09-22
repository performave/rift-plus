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

    // thrash x y amplitude cycles cps [mods] [left|right]
    //
    // What a hand does, rather than what is easy to assert about. `spam`
    // returns to its anchor before every release, so each cycle nets to zero
    // and any residue is the bug -- a clean measurement, and a gentle one.
    // Nobody spasming on a window edge releases where they pressed: the button
    // comes up mid-swing, the distances vary, the pauses vary, and some presses
    // barely move. This is irregular in every dimension a real gesture is.
    if (!strcmp(cmd, "thrash") && argc > 5) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        double amp = atof(argv[4]);
        int cycles = atoi(argv[5]);
        double cps = argc > 6 ? atof(argv[6]) : 9.0;
        CGEventFlags flags = argc > 7 ? parse_flags(argv[7]) : 0;
        int right = argc > 8 && !strcmp(argv[8], "right");
        CGMouseButton button = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
        CGEventType downType = right ? kCGEventRightMouseDown : kCGEventLeftMouseDown;
        CGEventType moveType = right ? kCGEventRightMouseDragged : kCGEventLeftMouseDragged;
        CGEventType upType = right ? kCGEventRightMouseUp : kCGEventLeftMouseUp;

        if (cps <= 0.0) cps = 9.0;
        useconds_t period = (useconds_t)(1000000.0 / cps);
        srandom(1);   // fixed, so a run repeats exactly
        post(kCGEventMouseMoved, a, 0, flags);
        usleep(20000);
        double at = 0.0;
        for (int c = 0; c < cycles; c++) {
            post(downType, CGPointMake(a.x + at, a.y), button, flags);
            int steps = 1 + (random() % 5);
            double to = at;
            for (int i = 0; i < steps; i++) {
                to = ((double)(random() % 2001) / 1000.0 - 1.0) * amp;
                post(moveType, CGPointMake(a.x + to, a.y), button, flags);
                usleep((period / 16) + (random() % (period / 8 + 1)));
            }
            post(upType, CGPointMake(a.x + to, a.y), button, flags);
            at = to;
            usleep((period / 4) + (random() % (period / 2 + 1)));
        }
        printf("thrashed %d cycles at ~%.1f/s, +/-%.0f from %.0f,%.0f button=%s\n",
               cycles, cps, amp, a.x, a.y, right ? "right" : "left");
        return 0;
    }

    // spam x y amplitude cycles cps [mods] [left|right]
    //
    // Whole press-drag-release cycles at a click rate, alternating direction
    // every cycle. `drag` cannot do this: it pads 80ms either side of the
    // button, so a loop around it tops out near 2.5 cycles a second, and the
    // report Eric is describing is 8-9 -- a hand spasming on a window edge.
    // What that rate buys is overlap: rift's writes for one drag, and the
    // app's notifications trailing them, are still in flight when the next
    // press captures the frame it will measure everything from.
    if (!strcmp(cmd, "spam") && argc > 5) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        double amp = atof(argv[4]);
        int cycles = atoi(argv[5]);
        double cps = argc > 6 ? atof(argv[6]) : 8.0;
        CGEventFlags flags = argc > 7 ? parse_flags(argv[7]) : 0;
        int right = argc > 8 && !strcmp(argv[8], "right");
        CGMouseButton button = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
        CGEventType downType = right ? kCGEventRightMouseDown : kCGEventLeftMouseDown;
        CGEventType moveType = right ? kCGEventRightMouseDragged : kCGEventLeftMouseDragged;
        CGEventType upType = right ? kCGEventRightMouseUp : kCGEventLeftMouseUp;

        if (cps <= 0.0) cps = 8.0;
        useconds_t period = (useconds_t)(1000000.0 / cps);
        post(kCGEventMouseMoved, a, 0, flags);
        usleep(20000);
        for (int c = 0; c < cycles; c++) {
            double to = (c % 2 == 0) ? amp : -amp;
            post(downType, a, button, flags);
            // Out and back, releasing where it started. A cycle that ends away
            // from its anchor leaves the window legitimately offset by that
            // last leg, so a run of them measures the final drag rather than
            // any accumulated error -- and "did it drift" is the whole
            // question. Returning to the anchor makes every cycle net zero by
            // construction, so anything left over is the bug.
            for (int i = 1; i <= 3; i++) {
                double x = a.x + to * ((double)i / 3.0);
                post(moveType, CGPointMake(x, a.y), button, flags);
                usleep(period / 8);
            }
            for (int i = 2; i >= 0; i--) {
                double x = a.x + to * ((double)i / 3.0);
                post(moveType, CGPointMake(x, a.y), button, flags);
                usleep(period / 8);
            }
            post(upType, a, button, flags);
            usleep(period / 4);
        }
        printf("spammed %d cycles at %.1f/s, +/-%.0f from %.0f,%.0f flags=0x%llx button=%s\n",
               cycles, cps, amp, a.x, a.y, (unsigned long long)flags,
               right ? "right" : "left");
        return 0;
    }

    // wiggle x y amplitude reversals [mods] [steps-per-leg] [left|right]
    //
    // One press, the pointer swung back and forth across `amplitude`, one
    // release. A drag that changes direction under the button is a different
    // thing from a run of separate drags: the reversal happens while the
    // gesture's own state is live, and a hand spasming on a window's edge
    // produces it constantly. Sending it as separate drags -- which is what a
    // loop around `drag` gives you -- tests the wrong thing.
    if (!strcmp(cmd, "wiggle") && argc > 5) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        double amp = atof(argv[4]);
        int reversals = atoi(argv[5]);
        CGEventFlags flags = argc > 6 ? parse_flags(argv[6]) : 0;
        int steps = argc > 7 ? atoi(argv[7]) : 6;
        if (steps < 1) steps = 1;
        int right = argc > 8 && !strcmp(argv[8], "right");
        CGMouseButton button = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
        CGEventType downType = right ? kCGEventRightMouseDown : kCGEventLeftMouseDown;
        CGEventType moveType = right ? kCGEventRightMouseDragged : kCGEventLeftMouseDragged;
        CGEventType upType = right ? kCGEventRightMouseUp : kCGEventLeftMouseUp;

        post(kCGEventMouseMoved, a, 0, flags);
        usleep(60000);
        post(downType, a, button, flags);
        usleep(40000);
        double at = 0.0;
        for (int leg = 0; leg < reversals; leg++) {
            double to = (leg % 2 == 0) ? amp : 0.0;
            for (int i = 1; i <= steps; i++) {
                double t = (double)i / steps;
                double x = at + (to - at) * t;
                post(moveType, CGPointMake(a.x + x, a.y), button, flags);
                usleep(8000);   // twice a hand's rate: the spasm case
            }
            at = to;
        }
        usleep(40000);
        post(upType, CGPointMake(a.x + at, a.y), button, flags);
        printf("wiggled from %.0f,%.0f by %.0f over %d reversals flags=0x%llx button=%s\n",
               a.x, a.y, amp, reversals, (unsigned long long)flags,
               right ? "right" : "left");
        return 0;
    }

    // path x y dx1,dx2,... [mods] [steps-per-leg] [left|right]
    //
    // One press, a polyline of horizontal offsets from the press point, one
    // release. `wiggle` reverses only between its amplitude and zero, which
    // can never ask the question this is for: what happens when a gesture
    // pushes a window past a size limit and then comes back *part* of the way.
    // Going all the way back lands on the origin either way, so a dead zone at
    // the limit is invisible to it. Here the turning points are given, so
    // "out to -600, back to -300" is expressible and the width at the end has
    // one right answer.
    if (!strcmp(cmd, "path") && argc > 4) {
        CGPoint a = CGPointMake(atof(argv[2]), atof(argv[3]));
        double legs[64];
        int n = 0;
        for (const char *p = argv[4]; *p && n < 64; ) {
            legs[n++] = atof(p);
            const char *c = strchr(p, ',');
            if (!c) break;
            p = c + 1;
        }
        if (n == 0) { fprintf(stderr, "path: no offsets\n"); return 1; }
        CGEventFlags flags = argc > 5 ? parse_flags(argv[5]) : 0;
        int steps = argc > 6 ? atoi(argv[6]) : 8;
        if (steps < 1) steps = 1;
        int right = argc > 7 && !strcmp(argv[7], "right");
        CGMouseButton button = right ? kCGMouseButtonRight : kCGMouseButtonLeft;
        CGEventType downType = right ? kCGEventRightMouseDown : kCGEventLeftMouseDown;
        CGEventType moveType = right ? kCGEventRightMouseDragged : kCGEventLeftMouseDragged;
        CGEventType upType = right ? kCGEventRightMouseUp : kCGEventLeftMouseUp;

        post(kCGEventMouseMoved, a, 0, flags);
        usleep(60000);
        post(downType, a, button, flags);
        usleep(40000);
        double at = 0.0;
        for (int leg = 0; leg < n; leg++) {
            double to = legs[leg];
            for (int i = 1; i <= steps; i++) {
                double t = (double)i / steps;
                post(moveType, CGPointMake(a.x + at + (to - at) * t, a.y), button, flags);
                usleep(12000);
            }
            at = to;
        }
        usleep(60000);
        post(upType, CGPointMake(a.x + at, a.y), button, flags);
        printf("path from %.0f,%.0f through %d leg(s) ending at %+.0f flags=0x%llx button=%s\n",
               a.x, a.y, n, at, (unsigned long long)flags, right ? "right" : "left");
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
                    "path <x> <y> <dx1,dx2,...> [mods] [steps] [left|right] | "
                    "gesture dock-swipe|processed [phase]\n");
    return 1;
} }
