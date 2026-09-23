// dtool -- display census and reconfiguration tracing.
//
//   dtool count           how many displays are online (fast; no swift compile)
//   dtool list            id/size/main/builtin per display, one per line
//   dtool mon <seconds>   log every CGDisplayReconfiguration callback with its
//                         flags, so an attach can be compared against what a
//                         real cable produces
//
// `mon` exists because a virtual attach might be unrealistically clean. A
// physical monitor negotiates its mode and flashes, firing several callbacks
// (BEGIN/MODE/SHAPE) around the ADD; if CGVirtualDisplay fires a bare ADD then
// the harness is testing an easier problem than the one that actually breaks.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>

static const struct { uint32_t bit; const char *name; } kFlags[] = {
    { kCGDisplayBeginConfigurationFlag, "BEGIN" },
    { kCGDisplayMovedFlag,              "MOVED" },
    { kCGDisplaySetMainFlag,            "SET_MAIN" },
    { kCGDisplaySetModeFlag,            "SET_MODE" },
    { kCGDisplayAddFlag,                "ADD" },
    { kCGDisplayRemoveFlag,             "REMOVE" },
    { kCGDisplayEnabledFlag,            "ENABLED" },
    { kCGDisplayDisabledFlag,           "DISABLED" },
    { kCGDisplayMirrorFlag,             "MIRROR" },
    { kCGDisplayUnMirrorFlag,           "UNMIRROR" },
    { kCGDisplayDesktopShapeChangedFlag,"DESKTOP_SHAPE_CHANGED" },
};

static double g_start;
static double now_ms(void) {
    return [[NSDate date] timeIntervalSince1970] * 1000.0;
}

static void on_reconfig(CGDirectDisplayID display, CGDisplayChangeSummaryFlags flags, void *ctx) {
    char buf[512]; buf[0] = 0;
    for (size_t i = 0; i < sizeof(kFlags)/sizeof(kFlags[0]); i++) {
        if (flags & kFlags[i].bit) {
            if (buf[0]) strlcat(buf, "|", sizeof buf);
            strlcat(buf, kFlags[i].name, sizeof buf);
        }
    }
    uint32_t n = 0; CGGetOnlineDisplayList(0, NULL, &n);
    printf("%8.0fms display=%-10u online=%u flags=0x%08x %s\n",
           now_ms() - g_start, display, n, (unsigned)flags, buf[0] ? buf : "(none)");
    fflush(stdout);
}

int main(int argc, const char **argv) { @autoreleasepool {
    const char *cmd = (argc > 1) ? argv[1] : "count";

    if (!strcmp(cmd, "count")) {
        uint32_t n = 0; CGGetOnlineDisplayList(0, NULL, &n);
        printf("%u\n", n);
        return 0;
    }

    if (!strcmp(cmd, "list")) {
        uint32_t n = 0; CGGetOnlineDisplayList(0, NULL, &n);
        CGDirectDisplayID ids[16];
        CGGetOnlineDisplayList(16, ids, &n);
        for (uint32_t i = 0; i < n; i++) {
            CGRect b = CGDisplayBounds(ids[i]);
            printf("%u %.0fx%.0f@%.0f,%.0f main=%d builtin=%d active=%d\n",
                   ids[i], b.size.width, b.size.height, b.origin.x, b.origin.y,
                   CGDisplayIsMain(ids[i]) != 0, CGDisplayIsBuiltin(ids[i]) != 0,
                   CGDisplayIsActive(ids[i]) != 0);
        }
        return 0;
    }

    if (!strcmp(cmd, "mon")) {
        double secs = (argc > 2) ? atof(argv[2]) : 20.0;
        g_start = now_ms();
        CGError err = CGDisplayRegisterReconfigurationCallback(on_reconfig, NULL);
        if (err != kCGErrorSuccess) { fprintf(stderr, "register failed: %d\n", err); return 2; }
        printf("monitoring %.0fs (online=%u at start)\n", secs, ({ uint32_t n=0; CGGetOnlineDisplayList(0,NULL,&n); n; }));
        fflush(stdout);
        [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:secs]];
        printf("done\n");
        return 0;
    }

    if (!strcmp(cmd, "setmain") && argc > 2) {
        // The display at the origin IS the main display; moving one to (0,0)
        // is how you hand it the menu bar. Real hardware fires SET_MAIN on a
        // replug when the external takes over, and a bare CGVirtualDisplay
        // attach never does -- so without this the display-reordering path
        // (and the remap opportunity it opens) goes untested.
        CGDirectDisplayID target = (CGDirectDisplayID)strtoul(argv[2], NULL, 0);
        CGDisplayConfigRef cfg;
        if (CGBeginDisplayConfiguration(&cfg) != kCGErrorSuccess) {
            fprintf(stderr, "CGBeginDisplayConfiguration failed\n"); return 2;
        }
        uint32_t n = 0; CGDirectDisplayID ids[16];
        CGGetOnlineDisplayList(16, ids, &n);
        CGRect tb = CGDisplayBounds(target);
        for (uint32_t i = 0; i < n; i++) {
            CGRect b = CGDisplayBounds(ids[i]);
            CGConfigureDisplayOrigin(cfg, ids[i],
                (int32_t)(b.origin.x - tb.origin.x), (int32_t)(b.origin.y - tb.origin.y));
        }
        CGError e = CGCompleteDisplayConfiguration(cfg, kCGConfigurePermanently);
        printf("setmain %u -> %s\n", target, e == kCGErrorSuccess ? "ok" : "failed");
        return e == kCGErrorSuccess ? 0 : 3;
    }

    if (!strcmp(cmd, "frames")) {
        // frames: every on-screen, layer-0 window as "<number> <x> <y> <w> <h>",
        // straight from the window server. What is actually on screen, as
        // against rift's own record of it, which keeps a window's last
        // reported frame until the app answers rift's next write.
        CFArrayRef list = CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
        for (NSDictionary *w in (__bridge NSArray *)list) {
            if ([w[(id)kCGWindowLayer] intValue] != 0) continue;
            CGRect b;
            if (!CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)w[(id)kCGWindowBounds], &b)) continue;
            printf("%u %.0f %.0f %.0f %.0f\n", [w[(id)kCGWindowNumber] unsignedIntValue],
                   b.origin.x, b.origin.y, b.size.width, b.size.height);
        }
        if (list) CFRelease(list);
        return 0;
    }

    if (!strcmp(cmd, "key") && argc > 2) {
        // key <keycode>: one key press, posted to the event stream. Escape
        // (53) closes Mission Control when it is open and does nothing when it
        // is not, which is the only way to put it in a known state: opening
        // it again toggles it, and nothing reports whether it is open.
        CGKeyCode code = (CGKeyCode)atoi(argv[2]);
        CGEventRef down = CGEventCreateKeyboardEvent(NULL, code, true);
        CGEventRef up = CGEventCreateKeyboardEvent(NULL, code, false);
        CGEventPost(kCGHIDEventTap, down);
        usleep(30000);
        CGEventPost(kCGHIDEventTap, up);
        CFRelease(down); CFRelease(up);
        printf("key %d\n", code);
        return 0;
    }

    if (!strcmp(cmd, "spaces")) {
        // spaces: each display's desktops in the window server's own order,
        // which is Mission Control's left to right. rift's space_ids is not
        // guaranteed to be in that order right after a move, and a test that
        // clicks thumbnails by position needs the one on screen.
        extern int CGSMainConnectionID(void);
        extern CFArrayRef CGSCopyManagedDisplaySpaces(int cid);
        CFArrayRef displays = CGSCopyManagedDisplaySpaces(CGSMainConnectionID());
        for (NSDictionary *d in (__bridge NSArray *)displays) {
            printf("%s", [d[@"Display Identifier"] UTF8String]);
            for (NSDictionary *sp in d[@"Spaces"]) printf(" %lld", [sp[@"ManagedSpaceID"] longLongValue]);
            printf("\n");
        }
        if (displays) CFRelease(displays);
        return 0;
    }

    if (!strcmp(cmd, "place") && argc > 4) {
        // place <display> <x> <y>: put one display at an origin in the global
        // space, the rest where they are. A probe always attaches to the
        // right, top-aligned; the report this exists for came from a laptop
        // to the LEFT of its monitor and bottom-aligned with it, which puts
        // the seam and the corners windows are parked in somewhere else.
        // Session-only, so a reboot puts the arrangement back.
        CGDirectDisplayID target = (CGDirectDisplayID)strtoul(argv[2], NULL, 0);
        CGDisplayConfigRef cfg;
        if (CGBeginDisplayConfiguration(&cfg) != kCGErrorSuccess) {
            fprintf(stderr, "CGBeginDisplayConfiguration failed\n"); return 2;
        }
        CGConfigureDisplayOrigin(cfg, target, atoi(argv[3]), atoi(argv[4]));
        CGError e = CGCompleteDisplayConfiguration(cfg, kCGConfigureForSession);
        CGRect b = CGDisplayBounds(target);
        printf("place %u -> %s, now at %.0f,%.0f %.0fx%.0f\n", target,
               e == kCGErrorSuccess ? "ok" : "failed",
               b.origin.x, b.origin.y, b.size.width, b.size.height);
        return e == kCGErrorSuccess ? 0 : 3;
    }

    if (!strcmp(cmd, "fullscreen")) {
        // Ctrl-Cmd-F, posted straight to the event stream. Apple Events are
        // the obvious way to do this and hang indefinitely inside a LaunchAgent
        // in the guest even with every relevant TCC grant in place, so this
        // goes underneath them: it is the same key the green button is.
        const CGKeyCode kF = 3;
        CGEventSourceRef src = CGEventSourceCreate(kCGEventSourceStateHIDSystemState);
        CGEventRef down = CGEventCreateKeyboardEvent(src, kF, true);
        CGEventRef up   = CGEventCreateKeyboardEvent(src, kF, false);
        CGEventFlags mods = kCGEventFlagMaskControl | kCGEventFlagMaskCommand;
        CGEventSetFlags(down, mods);
        CGEventSetFlags(up, mods);
        CGEventPost(kCGHIDEventTap, down);
        usleep(60000);
        CGEventPost(kCGHIDEventTap, up);
        if (down) CFRelease(down);
        if (up) CFRelease(up);
        if (src) CFRelease(src);
        printf("posted ctrl-cmd-f\n");
        return 0;
    }

    // modes [id] | setmode <width> <height> [id]
    //
    // The guest's display follows the VM window on the host, so a restart can
    // hand it a different size -- one came back 1216px wide instead of 2494,
    // and a churn battery on it measured nothing but five windows' minimum
    // sizes not fitting. These pick the size from inside the guest, with no
    // host involvement, so a run can be put back on a known footing first.
    if (!strcmp(cmd, "modes") || !strcmp(cmd, "setmode")) {
        int want_w = 0, want_h = 0;
        CGDirectDisplayID display = CGMainDisplayID();
        if (!strcmp(cmd, "setmode")) {
            if (argc < 4) { fprintf(stderr, "setmode <width> <height> [id]\n"); return 1; }
            want_w = atoi(argv[2]); want_h = atoi(argv[3]);
            if (argc > 4) display = (CGDirectDisplayID)strtoul(argv[4], NULL, 10);
        } else if (argc > 2) {
            display = (CGDirectDisplayID)strtoul(argv[2], NULL, 10);
        }
        const void *keys[] = { kCGDisplayShowDuplicateLowResolutionModes };
        const void *vals[] = { kCFBooleanTrue };
        CFDictionaryRef opts = CFDictionaryCreate(NULL, keys, vals, 1,
            &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
        CFArrayRef modes = CGDisplayCopyAllDisplayModes(display, opts);
        CFRelease(opts);
        if (!modes) { fprintf(stderr, "no modes for display %u\n", display); return 1; }
        CGDisplayModeRef pick = NULL;
        for (CFIndex i = 0; i < CFArrayGetCount(modes); i++) {
            CGDisplayModeRef m = (CGDisplayModeRef)CFArrayGetValueAtIndex(modes, i);
            size_t w = CGDisplayModeGetWidth(m), h = CGDisplayModeGetHeight(m);
            if (want_w == 0) {
                printf("%zux%zu pixels=%zux%zu\n", w, h,
                       CGDisplayModeGetPixelWidth(m), CGDisplayModeGetPixelHeight(m));
            } else if ((int)w == want_w && (int)h == want_h && !pick) {
                pick = m;
            }
        }
        if (want_w) {
            if (!pick) { fprintf(stderr, "no %dx%d mode\n", want_w, want_h); CFRelease(modes); return 1; }
            CGDisplayConfigRef cfg;
            CGBeginDisplayConfiguration(&cfg);
            CGConfigureDisplayWithDisplayMode(cfg, display, pick, NULL);
            CGError e = CGCompleteDisplayConfiguration(cfg, kCGConfigurePermanently);
            printf("setmode %dx%d on %u -> %s\n", want_w, want_h, display,
                   e == kCGErrorSuccess ? "ok" : "failed");
        }
        CFRelease(modes);
        return 0;
    }

    fprintf(stderr, "usage: dtool count|list|mon [seconds]|setmain <id>|fullscreen|"
                    "modes [id]|setmode <w> <h> [id]\n");
    return 1;
} }
