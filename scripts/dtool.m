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

    fprintf(stderr, "usage: dtool count|list|mon [seconds]|setmain <id>\n");
    return 1;
} }
