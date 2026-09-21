// vdisp -- hold a virtual display open for as long as this process lives.
//
// Attaching is `vdisp &`; detaching is killing it. That maps a real unplug
// onto process lifetime, which is the one teardown path that stays reliable:
// -[CGVirtualDisplay terminate] does not exist on macOS 27, and once a
// display's mode has been changed, releasing the object stops removing it. So
// the mode is set once, up front, and never touched again.
//
// Vendor/product/serial are fixed on purpose: every distinct virtual identity
// leaks a permanent root-owned ICC profile into
// /Library/ColorSync/Profiles/Displays.
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <objc/runtime.h>
#import <objc/message.h>

#define SET(obj, sel, type, val) do { \
    if ([obj respondsToSelector:NSSelectorFromString(@sel)]) \
        ((void(*)(id,SEL,type))objc_msgSend)(obj, NSSelectorFromString(@sel), val); \
} while(0)

static uint32_t online_count(void) {
    uint32_t n = 0; CGGetOnlineDisplayList(0, NULL, &n); return n;
}

int main(int argc, const char **argv) { @autoreleasepool {
    uint32_t w = (argc > 1) ? (uint32_t)atoi(argv[1]) : 1920;
    uint32_t h = (argc > 2) ? (uint32_t)atoi(argv[2]) : 1080;
    // A distinct serial gives a distinct display identity -- used by the
    // "different monitor, same port" scenario. Default stays fixed.
    uint32_t serial = (argc > 3) ? (uint32_t)strtoul(argv[3], NULL, 0) : 0x0001;

    Class Desc = NSClassFromString(@"CGVirtualDisplayDescriptor");
    Class Sett = NSClassFromString(@"CGVirtualDisplaySettings");
    Class Mode = NSClassFromString(@"CGVirtualDisplayMode");
    Class VD   = NSClassFromString(@"CGVirtualDisplay");
    if (!Desc || !Sett || !Mode || !VD) { fprintf(stderr, "SPI absent\n"); return 2; }

    id desc = [[Desc alloc] init];
    SET(desc, "setQueue:", dispatch_queue_t, dispatch_get_main_queue());
    SET(desc, "setName:", id, @"rift-vm-probe");
    SET(desc, "setMaxPixelsWide:", uint32_t, w);
    SET(desc, "setMaxPixelsHigh:", uint32_t, h);
    SET(desc, "setSizeInMillimeters:", CGSize, CGSizeMake(600, 340));
    SET(desc, "setProductID:", uint32_t, 0x1234);
    SET(desc, "setVendorID:",  uint32_t, 0x3456);
    SET(desc, "setSerialNum:", uint32_t, serial);

    uint32_t before = online_count();
    id vd = ((id(*)(id,SEL,id))objc_msgSend)([VD alloc],
             NSSelectorFromString(@"initWithDescriptor:"), desc);
    if (!vd) { fprintf(stderr, "initWithDescriptor: failed\n"); return 3; }

    id mode = ((id(*)(id,SEL,uint32_t,uint32_t,double))objc_msgSend)(
        [Mode alloc], NSSelectorFromString(@"initWithWidth:height:refreshRate:"), w, h, 60.0);
    id settings = [[Sett alloc] init];
    SET(settings, "setModes:", id, @[mode]);
    SET(settings, "setHiDPI:", uint32_t, 0);
    if (!((BOOL(*)(id,SEL,id))objc_msgSend)(vd, NSSelectorFromString(@"applySettings:"), settings)) {
        fprintf(stderr, "applySettings: refused\n"); return 4;
    }

    uint32_t after = before;
    for (int i = 0; i < 40 && after <= before; i++) { usleep(200000); after = online_count(); }
    if (after <= before) { fprintf(stderr, "no new display (still %u)\n", after); return 5; }

    uint32_t did = 0;
    if ([vd respondsToSelector:NSSelectorFromString(@"displayID")])
        did = ((uint32_t(*)(id,SEL))objc_msgSend)(vd, NSSelectorFromString(@"displayID"));
    printf("ATTACHED display_id=%u size=%ux%u serial=0x%x online=%u\n", did, w, h, serial, after);
    fflush(stdout);

    // Hold the strong reference. Dying is the detach.
    [[NSRunLoop currentRunLoop] run];
    (void)vd;
    return 0;
} }
