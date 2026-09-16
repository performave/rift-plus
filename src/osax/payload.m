//
// Vendored from yabai (https://github.com/koekeishiya/yabai), MIT-licensed;
// see LICENSE-yabai beside this file. Kept as close to upstream as possible so
// upstream fixes stay easy to pull in -- rift's changes are limited to the
// bundle path, the socket name and the log prefix.
//
#include <Foundation/Foundation.h>

#include <mach-o/getsect.h>
#include <mach-o/dyld.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <mach/vm_map.h>
#include <mach/vm_page_size.h>
#include <objc/message.h>
#include <objc/runtime.h>

#include <CoreGraphics/CoreGraphics.h>
#include <sys/types.h>
#include <sys/stat.h>
#include <sys/socket.h>
#include <arpa/inet.h>
#include <sys/un.h>
#include <unistd.h>
#include <netdb.h>
#include <dlfcn.h>
#include <mach/mach_time.h>
#include <math.h>
#include <libkern/OSCacheControl.h>

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>

#include "common.h"

#ifdef __x86_64__
#include "x64_payload.m"
#elif __arm64__
#include "arm64_payload.m"
#include <ptrauth.h>
#endif

#define HASHTABLE_IMPLEMENTATION
#include "hashtable.h"
#undef HASHTABLE_IMPLEMENTATION

#define page_align(addr) (vm_address_t)((uintptr_t)(addr) & (~(vm_page_size - 1)))
#define unpack(v) memcpy(&v, message, sizeof(v)); message += sizeof(v)
#define lerp(a, t, b) (((1.0-t)*a) + (t*b))

extern int SLSMainConnectionID(void);
extern CGError SLSGetConnectionPSN(int cid, ProcessSerialNumber *psn);
extern CGError SLSGetWindowAlpha(int cid, uint32_t wid, float *alpha);
extern CGError SLSSetWindowAlpha(int cid, uint32_t wid, float alpha);
extern OSStatus SLSMoveWindowWithGroup(int cid, uint32_t wid, CGPoint *point);
extern CGError SLSReassociateWindowsSpacesByGeometry(int cid, CFArrayRef window_list);
extern CGError SLSGetWindowOwner(int cid, uint32_t wid, int *window_cid);
extern CGError SLSSetWindowTags(int cid, uint32_t wid, uint64_t *tags, size_t tag_size);
extern CGError SLSClearWindowTags(int cid, uint32_t wid, uint64_t *tags, size_t tag_size);
extern CGError SLSGetWindowBounds(int cid, uint32_t wid, CGRect *frame);
extern CGError SLSGetWindowTransform(int cid, uint32_t wid, CGAffineTransform *t);
extern CGError SLSSetWindowTransform(int cid, uint32_t wid, CGAffineTransform t);
extern CGError SLSOrderWindow(int cid, uint32_t wid, int order, uint32_t rel_wid);
extern void SLSManagedDisplaySetCurrentSpace(int cid, CFStringRef display_ref, uint64_t sid);
extern uint64_t SLSManagedDisplayGetCurrentSpace(int cid, CFStringRef display_ref);
extern CFStringRef SLSCopyManagedDisplayForSpace(int cid, uint64_t sid);
extern void SLSMoveWindowsToManagedSpace(int cid, CFArrayRef window_list, uint64_t sid);
extern void SLSShowSpaces(int cid, CFArrayRef space_list);
extern void SLSHideSpaces(int cid, CFArrayRef space_list);
extern CFTypeRef SLSTransactionCreate(int cid);
extern CGError SLSTransactionCommit(CFTypeRef transaction, int synchronous);
extern CGError SLSTransactionOrderWindowGroup(CFTypeRef transaction, uint32_t wid, int order, uint32_t rel_wid);
extern CGError SLSTransactionSetWindowSystemAlpha(CFTypeRef transaction, uint32_t wid, float alpha);
extern CGError SLSSetWindowSubLevel(int cid, uint32_t wid, int level);

struct window_fade_context
{
    pthread_t thread;
    uint32_t wid;
    volatile float alpha;
    volatile float duration;
    volatile bool skip;
};

pthread_mutex_t window_fade_lock;
struct table window_fade_table;

static id dock_spaces;
static id dp_desktop_picture_manager;
static uint64_t add_space_fp;
static uint64_t remove_space_fp;
static uint64_t move_space_fp;
static uint64_t set_front_window_fp;
static uint64_t animation_time_addr;

// rift addition: Dock's trackpad space-switch step routine, hooked so rift can
// set the animation's timing. Zero when this Dock does not have it. See
// space_step_resolve and the block above init_instances.
static uint64_t space_step_addr;
static bool macOSSequoia;

static pthread_t daemon_thread;
static int daemon_sockfd;

static void dump_class_info(Class c)
{
    const char *name = class_getName(c);
    unsigned int count = 0;

    Ivar *ivar_list = class_copyIvarList(c, &count);
    for (int i = 0; i < count; i++) {
        Ivar ivar = ivar_list[i];
        const char *ivar_name = ivar_getName(ivar);
        NSLog(@"%s ivar: %s", name, ivar_name);
    }
    if (ivar_list) free(ivar_list);

    objc_property_t *property_list = class_copyPropertyList(c, &count);
    for (int i = 0; i < count; i++) {
        objc_property_t property = property_list[i];
        const char *prop_name = property_getName(property);
        NSLog(@"%s property: %s", name, prop_name);
    }
    if (property_list) free(property_list);

    Method *method_list = class_copyMethodList(c, &count);
    for (int i = 0; i < count; i++) {
        Method method = method_list[i];
        const char *method_name = sel_getName(method_getName(method));
        NSLog(@"%s method: %s", name, method_name);
    }
    if (method_list) free(method_list);
}

static Class dump_class_info_by_name(const char *name)
{
    Class c = objc_getClass(name);
    if (c != nil) {
        dump_class_info(c);
    }
    return c;
}

static uint64_t static_base_address(void)
{
    const struct segment_command_64 *command = getsegbyname("__TEXT");
    uint64_t addr = command->vmaddr;
    return addr;
}

static uint64_t image_slide(void)
{
    char path[1024];
    uint32_t size = sizeof(path);

    if (_NSGetExecutablePath(path, &size) != 0) {
        return -1;
    }

    for (uint32_t i = 0; i < _dyld_image_count(); i++) {
        if (strcmp(_dyld_get_image_name(i), path) == 0) {
            return _dyld_get_image_vmaddr_slide(i);
        }
    }

    return 0;
}

static uint64_t hex_find_seq(uint64_t baddr, const char *c_pattern)
{
    if (!baddr || !c_pattern) return 0;

    uint64_t addr = baddr;
    uint64_t pattern_length = (strlen(c_pattern) + 1) / 3;
    char buffer_a[pattern_length];
    char buffer_b[pattern_length];
    memset(buffer_a, 0, sizeof(buffer_a));
    memset(buffer_b, 0, sizeof(buffer_b));

    char *pattern = (char *) c_pattern + 1;
    for (int i = 0; i < pattern_length; ++i) {
        char c = pattern[-1];
        if (c == '?') {
            buffer_b[i] = 1;
        } else {
            int temp = c <= '9' ? 0 : 9;
            temp = (temp + c) << 0x4;
            c = pattern[0];
            int temp2 = c <= '9' ? 0xd0 : 0xc9;
            buffer_a[i] = temp2 + c + temp;
        }
        pattern += 3;
    }

loop:
    for (int counter = 0; counter < pattern_length; ++counter) {
        if ((buffer_b[counter] == 0) && (((char *)addr)[counter] != buffer_a[counter])) {
            addr = (uint64_t)((char *)addr + 1);
            if (addr - baddr < 0x1286a0) {
                goto loop;
            } else {
                return 0;
            }
        }
    }

    return addr;
}

#if __arm64__
uint64_t decode_adrp_add(uint64_t addr, uint64_t offset)
{
    uint32_t adrp_instr = *(uint32_t *) addr;

    uint32_t immlo = (0x60000000 & adrp_instr) >> 29;
    uint32_t immhi = (0xffffe0 & adrp_instr) >> 3;

    int32_t value = (immhi | immlo) << 12;
    int64_t value_64 = value;

    uint32_t add_instr = *(uint32_t *) (addr + 4);
    uint64_t imm12 = (add_instr & 0x3ffc00) >> 10;

    if (add_instr & 0xc00000) {
        imm12 <<= 12;
    }

    return (offset & 0xfffffffffffff000) + value_64 + imm12;
}
#endif

static bool verify_os_version(NSOperatingSystemVersion os_version)
{
    NSLog(@"[rift-sa] checking for macOS %ld.%ld.%ld compatibility!", os_version.majorVersion, os_version.minorVersion, os_version.patchVersion);

#ifdef __x86_64__
    if (os_version.majorVersion == 11) {
        return true; // Big Sur 11.0
    } else if (os_version.majorVersion == 12) {
        return true; // Monterey 12.0
    } else if (os_version.majorVersion == 13) {
        return true; // Ventura 13.0
    } else if (os_version.majorVersion == 14) {
        return true; // Sonoma 14.0
    } else if (os_version.majorVersion == 15) {
        macOSSequoia = true;
        return true; // Sequoia 15.0
    } else if (os_version.majorVersion == 26) {

        NSLog(@"[rift-sa] Detected Tahoe Preview... flagging 'macOSSequoia=true.'");
        macOSSequoia = true;
        return true; // Tahoe preview
    }

    NSLog(@"[rift-sa] spaces functionality is only supported on macOS Big Sur 11.0.0+, Monterey 12.0.0+, Ventura 13.0.0+, Sonoma 14.0.0+, and Sequoia 15.0");
#elif __arm64__
    if (os_version.majorVersion == 12) {
        return true; // Monterey 12.0
    } else if (os_version.majorVersion == 13) {
        return true; // Ventura 13.0
    } else if (os_version.majorVersion == 14) {
        return true; // Sonoma 14.0
    } else if (os_version.majorVersion == 15) {
        macOSSequoia = true;
        return true; // Sequoia 15.0
    } else if (os_version.majorVersion == 26) {

        NSLog(@"[rift-sa] Detected Tahoe Preview... flagging 'macOSSequoia=true.'");
        macOSSequoia = true;
        return true; // Tahoe preview
    } else if (os_version.majorVersion == 27) {
        //
        // 27 kept Tahoe's Dock internals: every byte pattern below still
        // matches this Dock, and only the offsets the search starts from
        // moved. The two selector spellings macOSSequoia picks between are
        // unchanged, so it is set here for the same reason as on 26.
        //
        NSLog(@"[rift-sa] Detected macOS 27... flagging 'macOSSequoia=true.'");
        macOSSequoia = true;
        return true; // macOS 27
    }

    NSLog(@"[rift-sa] spaces functionality is only supported on macOS Monterey 12.0.0+, and Ventura 13.0.0+, Sonoma 14.0.0+, and Sequoia 15.0");
#endif

    return false;
}

#if __arm64__
//
// rift addition: custom timing for the trackpad space switch.
//
// When a swipe between spaces is released, Dock animates the rest of the way
// with a velocity spring that a routine on DockCore.SpaceSwitcher steps from a
// timer: it reads the target space index and the current scroll position,
// integrates the spring, stores the new position, pushes it to the window
// server and answers whether the animation is over. There is no duration in
// it to change, and its coefficients sit in a literal pool that three other
// Dock animations share, so the routine's body is hooked instead. Its
// prologue and its tail are left alone; the math between them is replaced by
// a jump into space_step_thunk, which asks space_step for the position and
// jumps back to the tail with it, so Dock still stores, applies and commits
// exactly as before. Dock keeps tracking the finger and rendering; only the
// curve after the fingers lift is rift's, a duration and a cubic bezier that
// arrive on SA_OPCODE_SPACE_SWITCH_ANIMATION.
//
// Nothing is patched until rift asks. Until then, and again after a duration
// of zero puts the original bytes back, the routine is Dock's own.
//

// The five prologue instructions stay; the detour goes where `mov v8, v0` was.
#define SPACE_STEP_DETOUR_OFFSET 0x14
// ldr x16, #8 ; br x16 ; .quad space_step_thunk
#define SPACE_STEP_PATCH_LEN     16

static uint64_t space_step_resume_delta;
static uint8_t space_step_original[SPACE_STEP_PATCH_LEN];
static bool space_step_hooked;

static ptrdiff_t space_step_off_position;
static ptrdiff_t space_step_off_index;
static ptrdiff_t space_step_off_count;
static ptrdiff_t space_step_off_last_time;
static ptrdiff_t space_step_off_velocity;

static double space_step_duration;
static double space_step_bezier[4];

static struct {
    bool active;
    double start_time;
    double start_position;
    double target;
    double expected_position;
} space_step_anim;

// Read from the thunk, so it has to survive the optimizer as a real symbol.
__attribute__((used, visibility("hidden"))) uint64_t space_step_resume;

// CACurrentMediaTime without QuartzCore: the same clock, in seconds, which is
// what Dock keeps in lastAnimationTime.
static double space_step_now(void)
{
    static mach_timebase_info_data_t timebase;
    if (timebase.denom == 0) mach_timebase_info(&timebase);
    return (double) mach_absolute_time() * timebase.numer / timebase.denom / 1e9;
}

// y at x on the cubic bezier through (0,0), (x1,y1), (x2,y2), (1,1): the CSS
// timing function. Newton on the x polynomial, bisection if it strays.
static double space_step_ease(double x)
{
    if (x <= 0.0) return 0.0;
    if (x >= 1.0) return 1.0;

    double x1 = space_step_bezier[0], y1 = space_step_bezier[1];
    double x2 = space_step_bezier[2], y2 = space_step_bezier[3];
    double cx = 3.0 * x1, bx = 3.0 * (x2 - x1) - cx, ax = 1.0 - cx - bx;
    double cy = 3.0 * y1, by = 3.0 * (y2 - y1) - cy, ay = 1.0 - cy - by;

    double t = x;
    bool converged = false;
    for (int i = 0; i < 8; ++i) {
        double error = ((ax * t + bx) * t + cx) * t - x;
        if (fabs(error) < 1e-6) { converged = true; break; }
        double slope = (3.0 * ax * t + 2.0 * bx) * t + cx;
        if (fabs(slope) < 1e-9) break;
        t -= error / slope;
    }
    if (!converged || t < 0.0 || t > 1.0) {
        double lo = 0.0, hi = 1.0;
        t = x;
        for (int i = 0; i < 32; ++i) {
            double xt = ((ax * t + bx) * t + cx) * t;
            if (fabs(xt - x) < 1e-6) break;
            if (xt < x) lo = t; else hi = t;
            t = (lo + hi) * 0.5;
        }
    }
    return ((ay * t + by) * t + cy) * t;
}

// The replacement for the spring. Runs inside Dock's own frame for the
// routine, on its animation timer, with `self` the SpaceSwitcher and `dt` the
// timestep Dock passes (unused: the curve is over wall-clock time). Writes the
// new scroll position and returns whether the animation is finished, which
// the tail stores and returns as the routine always did.
__attribute__((used, visibility("hidden"))) int space_step(void *self, double dt, double *out_position)
{
    (void) dt;
    char *fields = (char *) self;
    double position = *(double *) (fields + space_step_off_position);
    int64_t index = *(int64_t *) (fields + space_step_off_index);
    int64_t count = *(int64_t *) (fields + space_step_off_count);
    double target = count >= 2 ? (double) index / (double) (count - 1) : 0.0;
    double now = space_step_now();

    // A fresh animation: the first step after a release, a release toward a
    // different space, or a position Dock moved without us -- the finger
    // came back down and dragged the spaces itself.
    if (!space_step_anim.active
        || space_step_anim.target != target
        || position != space_step_anim.expected_position) {
        space_step_anim.active = true;
        space_step_anim.start_time = now;
        space_step_anim.start_position = position;
        space_step_anim.target = target;
    }

    double t = space_step_duration > 0.0 ? (now - space_step_anim.start_time) / space_step_duration : 1.0;
    bool finished = t >= 1.0;
    double next = finished
                ? target
                : space_step_anim.start_position + (target - space_step_anim.start_position) * space_step_ease(t);

    *(double *) (fields + space_step_off_last_time) = now;
    if (finished) {
        *(double *) (fields + space_step_off_velocity) = 0.0;
        space_step_anim.active = false;
    }
    space_step_anim.expected_position = next;
    *out_position = next;
    return finished ? 1 : 0;
}

// Entered from the detour with Dock's frame in place: x20 is self, d0 the
// timestep, and x19, d1 are what the tail expects -- the finished flag and
// the new position. x19 and x20 are callee-saved, so they survive the call;
// x30 was saved by the prologue and is restored by the epilogue.
__attribute__((naked)) static void space_step_thunk(void)
{
    __asm__ volatile(
        "sub sp, sp, #16\n"
        "mov x0, x20\n"
        "mov x1, sp\n"
        "bl _space_step\n"
        "mov w19, w0\n"
        "ldr d1, [sp]\n"
        "add sp, sp, #16\n"
        "adrp x16, _space_step_resume@PAGE\n"
        "ldr x16, [x16, _space_step_resume@PAGEOFF]\n"
        "br x16\n"
    );
}

static bool space_step_write(uint64_t addr, const void *bytes, size_t len)
{
    vm_address_t start = page_align(addr);
    vm_size_t size = page_align(addr + len - 1) + vm_page_size - start;
    if (vm_protect(mach_task_self(), start, size, 0, VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY) != KERN_SUCCESS) {
        return false;
    }
    memcpy((void *) addr, bytes, len);
    vm_protect(mach_task_self(), start, size, 0, VM_PROT_READ | VM_PROT_EXECUTE);
    sys_icache_invalidate((void *) addr, len);
    return true;
}

// Installs or removes the detour. Called on the main queue, which is where
// Dock's animation timer runs, so the routine is not mid-flight while its
// bytes change.
static void space_step_set_hooked(bool hooked)
{
    if (!space_step_addr || hooked == space_step_hooked) return;
    uint64_t site = space_step_addr + SPACE_STEP_DETOUR_OFFSET;

    if (hooked) {
        uint8_t patch[SPACE_STEP_PATCH_LEN];
        uint32_t ldr = 0x58000050; // ldr x16, #8
        uint32_t br = 0xD61F0200;  // br x16
        uint64_t thunk = (uint64_t) ptrauth_strip((void *) &space_step_thunk, ptrauth_key_asia);
        memcpy(patch, &ldr, sizeof(ldr));
        memcpy(patch + 4, &br, sizeof(br));
        memcpy(patch + 8, &thunk, sizeof(thunk));
        memcpy(space_step_original, (void *) site, SPACE_STEP_PATCH_LEN);
        if (space_step_write(site, patch, sizeof(patch))) {
            space_step_hooked = true;
            NSLog(@"[rift-sa] space switch step hooked at 0x%llx", site);
        } else {
            NSLog(@"[rift-sa] space switch step vm_protect failed; unable to hook!");
        }
    } else {
        if (space_step_write(site, space_step_original, SPACE_STEP_PATCH_LEN)) {
            space_step_hooked = false;
            NSLog(@"[rift-sa] space switch step restored");
        } else {
            NSLog(@"[rift-sa] space switch step vm_protect failed; unable to restore!");
        }
    }
}

static void space_step_resolve(uint64_t baseaddr, NSOperatingSystemVersion os_version)
{
    const char *pattern = get_space_step_pattern(os_version);
    if (!pattern) return;

    uint64_t addr = hex_find_seq(baseaddr + get_space_step_offset(os_version), pattern);
    if (addr == 0) {
        NSLog(@"[rift-sa] failed to get pointer to space switch step..");
        return;
    }

    // The routine reads the switcher through fixed offsets and so does the
    // hook, but the hook takes them from the runtime rather than from a
    // table, since the Swift class still lists them as ivars.
    Class switcher = objc_getClass("_TtC8DockCore13SpaceSwitcher");
    struct { const char *name; ptrdiff_t *offset; } ivars[] = {
        { "scrollPosition",    &space_step_off_position },
        { "currentSpaceIndex", &space_step_off_index },
        { "_spacesCount",      &space_step_off_count },
        { "lastAnimationTime", &space_step_off_last_time },
        { "_velocity",         &space_step_off_velocity },
    };
    for (size_t i = 0; i < sizeof(ivars) / sizeof(ivars[0]); ++i) {
        Ivar ivar = switcher ? class_getInstanceVariable(switcher, ivars[i].name) : NULL;
        if (!ivar) {
            NSLog(@"[rift-sa] SpaceSwitcher has no ivar %s; space switch step will not be hooked", ivars[i].name);
            return;
        }
        *ivars[i].offset = ivar_getOffset(ivar);
    }

    // The tail the hook returns to has to be `str d1, [x20, #scrollPosition]`
    // -- the one store the hook feeds -- or the resume delta is for a
    // different Dock than this one.
    uint64_t resume = addr + get_space_step_resume_delta(os_version);
    uint32_t expected = 0xFD000000 | ((uint32_t) (space_step_off_position / 8) << 10) | (20 << 5) | 1;
    if (*(uint32_t *) resume != expected) {
        NSLog(@"[rift-sa] space switch step tail at 0x%llx is 0x%x, expected 0x%x; not hooking", resume, *(uint32_t *) resume, expected);
        return;
    }

    space_step_addr = addr;
    space_step_resume = resume;
    space_step_resume_delta = resume - addr;
    NSLog(@"[rift-sa] (0x%llx) space switch step found at address 0x%llX (0x%llx)", baseaddr, addr, addr - baseaddr);
}
#else
static void space_step_resolve(uint64_t baseaddr, NSOperatingSystemVersion os_version)
{
    (void) baseaddr;
    (void) os_version;
}
#endif

static void init_instances()
{
    NSOperatingSystemVersion os_version = [[NSProcessInfo processInfo] operatingSystemVersion];
    if (!verify_os_version(os_version)) return;

    uint64_t baseaddr = static_base_address() + image_slide();

    uint64_t dock_spaces_addr = hex_find_seq(baseaddr + get_dock_spaces_offset(os_version), get_dock_spaces_pattern(os_version));
    if (dock_spaces_addr == 0) {
        dock_spaces = nil;
        NSLog(@"[rift-sa] could not locate pointer to dock.spaces! spaces functionality will not work!");
    } else {
#ifdef __x86_64__
        uint32_t dock_spaces_offset = *(int32_t *)dock_spaces_addr;
        NSLog(@"[rift-sa] (0x%llx) dock.spaces found at address 0x%llX (0x%llx)", baseaddr, dock_spaces_addr, dock_spaces_addr - baseaddr);
        dock_spaces = [(*(id *)(dock_spaces_addr + dock_spaces_offset + 0x4)) retain];
#elif __arm64__
        uint64_t dock_spaces_offset = decode_adrp_add(dock_spaces_addr, dock_spaces_addr - baseaddr);
        NSLog(@"[rift-sa] (0x%llx) dock.spaces found at address 0x%llX (0x%llx)", baseaddr, dock_spaces_offset, dock_spaces_offset - baseaddr);
        dock_spaces = [(*(id *)(baseaddr + dock_spaces_offset)) retain];
#endif
    }

    uint64_t dppm_addr = hex_find_seq(baseaddr + get_dppm_offset(os_version), get_dppm_pattern(os_version));
    if (dppm_addr == 0) {
        dp_desktop_picture_manager = nil;
        NSLog(@"[rift-sa] could not locate pointer to dppm! moving spaces will not work!");
    } else {
#ifdef __x86_64__
        uint32_t dppm_offset = *(int32_t *)dppm_addr;
        NSLog(@"[rift-sa] (0x%llx) dppm found at address 0x%llX (0x%llx)", baseaddr, dppm_addr, dppm_addr - baseaddr);
        dp_desktop_picture_manager = [(*(id *)(dppm_addr + dppm_offset + 0x4)) retain];
#elif __arm64__
        uint64_t dppm_offset = decode_adrp_add(dppm_addr, dppm_addr - baseaddr);
        NSLog(@"[rift-sa] (0x%llx) dppm found at address 0x%llX (0x%llx)", baseaddr, dppm_offset, dppm_offset - baseaddr);
        dp_desktop_picture_manager = [(*(id *)(baseaddr + dppm_offset)) retain];
#endif

        //
        // @hack
        //
        // NOTE(asmvik): For whatever reason, in Sonoma, DPDesktopPictureManager is initialized and swapped
        // to an alternate storage location instead of where it used to be stored in previous macOS versions..
        //
        // This alternate storage location resides 8-bytes before the usual location, so we simply do
        // the subtract to arrive at the correct location in cases where the usual location is null.
        //

#ifdef __x86_64__
        if (dp_desktop_picture_manager == nil) {
            dp_desktop_picture_manager = [(*(id *)(dppm_addr + dppm_offset + 0x4 - 0x8)) retain];
        }
#elif __arm64__
        if (dp_desktop_picture_manager == nil) {
            dp_desktop_picture_manager = [(*(id *)(baseaddr + dppm_offset - 0x8)) retain];
        }
#endif
    }

    uint64_t add_space_addr = hex_find_seq(baseaddr + get_add_space_offset(os_version), get_add_space_pattern(os_version));
    if (add_space_addr == 0x0) {
        NSLog(@"[rift-sa] failed to get pointer to addSpace function..");
        add_space_fp = 0;
    } else {
        NSLog(@"[rift-sa] (0x%llx) addSpace found at address 0x%llX (0x%llx)", baseaddr, add_space_addr, add_space_addr - baseaddr);
#ifdef __x86_64__
        add_space_fp = add_space_addr;
#elif __arm64__
        add_space_fp = (uint64_t) ptrauth_sign_unauthenticated((void *) add_space_addr, ptrauth_key_asia, 0);
#endif
    }

    uint64_t remove_space_addr = hex_find_seq(baseaddr + get_remove_space_offset(os_version), get_remove_space_pattern(os_version));
    if (remove_space_addr == 0x0) {
        NSLog(@"[rift-sa] failed to get pointer to removeSpace function..");
        remove_space_fp = 0;
    } else {
        NSLog(@"[rift-sa] (0x%llx) removeSpace found at address 0x%llX (0x%llx)", baseaddr, remove_space_addr, remove_space_addr - baseaddr);
#ifdef __x86_64__
        remove_space_fp = remove_space_addr;
#elif __arm64__
        remove_space_fp = (uint64_t) ptrauth_sign_unauthenticated((void *) remove_space_addr, ptrauth_key_asia, 0);
#endif
    }

    uint64_t move_space_addr = hex_find_seq(baseaddr + get_move_space_offset(os_version), get_move_space_pattern(os_version));
    if (move_space_addr == 0x0) {
        NSLog(@"[rift-sa] failed to get pointer to moveSpace function..");
        move_space_fp = 0;
    } else {
        NSLog(@"[rift-sa] (0x%llx) moveSpace found at address 0x%llX (0x%llx)", baseaddr, move_space_addr, move_space_addr - baseaddr);
#ifdef __x86_64__
        move_space_fp = move_space_addr;
#elif __arm64__
        move_space_fp = (uint64_t) ptrauth_sign_unauthenticated((void *) move_space_addr, ptrauth_key_asia, 0);
#endif
    }

    uint64_t set_front_window_addr = hex_find_seq(baseaddr + get_set_front_window_offset(os_version), get_set_front_window_pattern(os_version));
    if (set_front_window_addr == 0x0) {
        NSLog(@"[rift-sa] failed to get pointer to setFrontWindow function..");
        set_front_window_fp = 0;
    } else {
        NSLog(@"[rift-sa] (0x%llx) setFrontWindow found at address 0x%llX (0x%llx)", baseaddr, set_front_window_addr, set_front_window_addr - baseaddr);
#ifdef __x86_64__
        set_front_window_fp = set_front_window_addr;
#elif __arm64__
        set_front_window_fp = (uint64_t) ptrauth_sign_unauthenticated((void *) set_front_window_addr, ptrauth_key_asia, 0);
#endif
    }

    animation_time_addr = hex_find_seq(baseaddr + get_fix_animation_offset(os_version), get_fix_animation_pattern(os_version));
    if (animation_time_addr == 0x0) {
        NSLog(@"[rift-sa] failed to get pointer to animation-time..");
    } else {
        NSLog(@"[rift-sa] (0x%llx) animation_time_addr found at address 0x%llX (0x%llx)", baseaddr, animation_time_addr, animation_time_addr - baseaddr);
        if (vm_protect(mach_task_self(), page_align(animation_time_addr), vm_page_size, 0, VM_PROT_READ | VM_PROT_WRITE | VM_PROT_COPY) == KERN_SUCCESS) {
#ifdef __x86_64__
            *(uint64_t *) animation_time_addr = 0x660fefc0660fefc0;
#elif __arm64__
            *(uint32_t *) animation_time_addr = 0x2f00e400;
#endif
            vm_protect(mach_task_self(), page_align(animation_time_addr), vm_page_size, 0, VM_PROT_READ | VM_PROT_EXECUTE);
        } else {
            NSLog(@"[rift-sa] animation_time_addr vm_protect failed; unable to patch instruction!");
        }
    }

    space_step_resolve(baseaddr, os_version);
}

static inline id get_ivar_value(id instance, const char *name)
{
    id result = nil;
    object_getInstanceVariable(instance, name, (void **) &result);
    return result;
}

static inline void set_ivar_value(id instance, const char *name, id value)
{
    object_setInstanceVariable(instance, name, value);
}

static inline uint64_t get_space_id(id space)
{
    return ((uint64_t (*)(id, SEL)) objc_msgSend)(space, @selector(spid));
}

static inline id space_for_display_with_id(CFStringRef display_uuid, uint64_t space_id)
{
    // NOTE(rift): a space the window server no longer knows has no display;
    // its uuid comes back NULL, and Dock's lookups must not see it.
    if (display_uuid == NULL) return nil;
    NSArray *spaces_for_display = ((NSArray *(*)(id, SEL, CFStringRef)) objc_msgSend)(dock_spaces, @selector(spacesForDisplay:), display_uuid);
    for (id space in spaces_for_display) {
        if (space_id == get_space_id(space)) {
            return space;
        }
    }
    return nil;
}

static inline id display_space_for_display_uuid(CFStringRef display_uuid)
{
    id result = nil;
    if (display_uuid == NULL) return nil;

    NSArray *display_spaces = get_ivar_value(dock_spaces, "_displaySpaces");
    if (display_spaces != nil) {
        for (id display_space in display_spaces) {
            id display_source_space = get_ivar_value(display_space, "_currentSpace");
            uint64_t sid = get_space_id(display_source_space);
            CFStringRef uuid = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), sid);
            if (uuid == NULL) continue;
            bool match = CFEqual(uuid, display_uuid);
            CFRelease(uuid);
            if (match) {
                result = display_space;
                break;
            }
        }
    }

    return result;
}

static inline id display_space_for_space_with_id(uint64_t space_id)
{
    NSArray *display_spaces = get_ivar_value(dock_spaces, "_displaySpaces");
    if (display_spaces != nil) {
        for (id display_space in display_spaces) {
            id display_source_space = get_ivar_value(display_space, "_currentSpace");
            if (get_space_id(display_source_space) == space_id) {
                return display_space;
            }
        }
    }
    return nil;
}

static void do_space_move(char *message)
{
    if (dock_spaces == nil || dp_desktop_picture_manager == nil || move_space_fp == 0) return;

    uint64_t source_space_id, dest_space_id, source_prev_space_id;
    unpack(source_space_id);
    unpack(dest_space_id);
    unpack(source_prev_space_id);

    bool focus_dest_space;
    unpack(focus_dest_space);

    CFStringRef source_display_uuid = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), source_space_id);
    CFStringRef dest_display_uuid = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), dest_space_id);
    // NOTE(rift): either space gone (a destroyed desktop) is a move with
    // nothing to do, not a crash inside Dock.
    if (source_display_uuid == NULL || dest_display_uuid == NULL) {
        if (source_display_uuid) CFRelease(source_display_uuid);
        if (dest_display_uuid) CFRelease(dest_display_uuid);
        return;
    }
    id source_space = space_for_display_with_id(source_display_uuid, source_space_id);
    id source_display_space = display_space_for_display_uuid(source_display_uuid);

    id dest_space = space_for_display_with_id(dest_display_uuid, dest_space_id);
    unsigned dest_display_id = ((unsigned (*)(id, SEL, id)) objc_msgSend)(dock_spaces, @selector(displayIDForSpace:), dest_space);
    id dest_display_space = display_space_for_display_uuid(dest_display_uuid);

    if (source_prev_space_id) {
        NSArray *ns_source_space = @[ @(source_space_id) ];
        NSArray *ns_dest_space = @[ @(source_prev_space_id) ];
        id new_source_space = space_for_display_with_id(source_display_uuid, source_prev_space_id);
        SLSShowSpaces(SLSMainConnectionID(), (__bridge CFArrayRef) ns_dest_space);
        SLSHideSpaces(SLSMainConnectionID(), (__bridge CFArrayRef) ns_source_space);
        SLSManagedDisplaySetCurrentSpace(SLSMainConnectionID(), source_display_uuid, source_prev_space_id);
        set_ivar_value(source_display_space, "_currentSpace", [new_source_space retain]);
        [ns_dest_space release];
        [ns_source_space release];
    }

    asm__call_move_space(source_space, dest_space, dest_display_uuid, dock_spaces, move_space_fp);

    dispatch_sync(dispatch_get_main_queue(), ^{
        ((void (*)(id, SEL, id, unsigned, CFStringRef)) objc_msgSend)(dp_desktop_picture_manager, @selector(moveSpace:toDisplay:displayUUID:), source_space, dest_display_id, dest_display_uuid);
    });

    if (focus_dest_space) {
        uint64_t new_source_space_id = SLSManagedDisplayGetCurrentSpace(SLSMainConnectionID(), source_display_uuid);
        id new_source_space = space_for_display_with_id(source_display_uuid, new_source_space_id);
        set_ivar_value(source_display_space, "_currentSpace", [new_source_space retain]);

        NSArray *ns_dest_monitor_space = @[ @(dest_space_id) ];
        SLSHideSpaces(SLSMainConnectionID(), (__bridge CFArrayRef) ns_dest_monitor_space);
        SLSManagedDisplaySetCurrentSpace(SLSMainConnectionID(), dest_display_uuid, source_space_id);
        set_ivar_value(dest_display_space, "_currentSpace", [source_space retain]);
        [ns_dest_monitor_space release];
    }

    CFRelease(source_display_uuid);
    CFRelease(dest_display_uuid);
}

typedef void (*remove_space_call)(id space, id display_space, id dock_spaces, uint64_t space_id1, uint64_t space_id2);
static void do_space_destroy(char *message)
{
    if (dock_spaces == nil || remove_space_fp == 0) return;

    uint64_t space_id;
    unpack(space_id);

    CFStringRef display_uuid = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), space_id);
    if (display_uuid == NULL) return;
    uint64_t active_space_id = SLSManagedDisplayGetCurrentSpace(SLSMainConnectionID(), display_uuid);

    id space = space_for_display_with_id(display_uuid, space_id);
    id display_space = display_space_for_display_uuid(display_uuid);

    dispatch_sync(dispatch_get_main_queue(), ^{
        ((remove_space_call) remove_space_fp)(space, display_space, dock_spaces, space_id, space_id);
    });

    if (active_space_id == space_id) {
        uint64_t dest_space_id = SLSManagedDisplayGetCurrentSpace(SLSMainConnectionID(), display_uuid);
        id dest_space = space_for_display_with_id(display_uuid, dest_space_id);
        set_ivar_value(display_space, "_currentSpace", [dest_space retain]);
    }

    CFRelease(display_uuid);
}

static void do_space_create(char *message)
{
    if (dock_spaces == nil || add_space_fp == 0) return;

    uint64_t space_id;
    unpack(space_id);

    CFStringRef __block display_uuid = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), space_id);
    if (display_uuid == NULL) return;
    dispatch_sync(dispatch_get_main_queue(), ^{
        id new_space = macOSSequoia
                     ? [[objc_getClass("ManagedSpace") alloc] init]
                     : [[objc_getClass("Dock.ManagedSpace") alloc] init];
        id display_space = display_space_for_display_uuid(display_uuid);
        asm__call_add_space(new_space, display_space, add_space_fp);
        CFRelease(display_uuid);
    });
}

//
// NOTE(rift): unlike yabai, this reports whether the switch was issued. The
// client falls back to a synthetic swipe when it was not, and a swipe on top
// of a switch that did happen lands one space too far, so the answer has to
// come from here rather than from watching the window server afterwards.
// "Already there" counts as done: there is nothing for a fallback to do.
//
// Dock shows and hides itself around fullscreen spaces from a visibility
// controller that keeps its own idea of which space the bar is on, and only
// Dock's own switch transition hands that controller the new space. The SLS
// calls in do_space_focus move the display without that transition, so a
// switch out of a fullscreen space arrived with the bar still hidden and one
// into a fullscreen space left it up over the app. This is the call Dock's
// own space-change listener makes: the spaces object's delegate is the
// DockBar, which forwards to the controller, and the controller ignores a
// space on a display the Dock is not on. It goes to the main queue like the
// other Dock mutations here; the switch has already been issued by then, so
// the verdict does not wait on it.
static void dock_update_visibility_for_space(id space)
{
    if (![dock_spaces respondsToSelector:@selector(delegate)]) return;
    id delegate = ((id (*)(id, SEL)) objc_msgSend)(dock_spaces, @selector(delegate));
    if (delegate == nil) return;

    SEL update = @selector(setDockVisibleOnSpace:alreadyLocked:);
    if (![delegate respondsToSelector:update]) return;

    [delegate retain];
    [space retain];
    dispatch_async(dispatch_get_main_queue(), ^{
        ((void (*)(id, SEL, id, BOOL)) objc_msgSend)(delegate, update, space, NO);
        [space release];
        [delegate release];
    });
}

static bool do_space_focus(char *message)
{
    if (dock_spaces == nil) return false;

    uint64_t dest_space_id;
    unpack(dest_space_id);
    if (!dest_space_id) return false;

    bool focused = false;
    CFStringRef dest_display = SLSCopyManagedDisplayForSpace(SLSMainConnectionID(), dest_space_id);
    if (dest_display == NULL) return false;
    id source_space = macOSSequoia
                    ? ((id (*)(id, SEL, CFStringRef)) objc_msgSend)(dock_spaces, @selector(currentSpaceForDisplayUUID:), dest_display)
                    : ((id (*)(id, SEL, CFStringRef)) objc_msgSend)(dock_spaces, @selector(currentSpaceforDisplayUUID:), dest_display);
    uint64_t source_space_id = get_space_id(source_space);

    if (source_space_id == dest_space_id) {
        focused = true;
    } else {
        id dest_space = space_for_display_with_id(dest_display, dest_space_id);
        if (dest_space != nil) {
            id display_space = display_space_for_space_with_id(source_space_id);
            if (display_space != nil) {
                NSArray *ns_source_space = @[ @(source_space_id) ];
                NSArray *ns_dest_space = @[ @(dest_space_id) ];
                SLSShowSpaces(SLSMainConnectionID(), (__bridge CFArrayRef) ns_dest_space);
                SLSHideSpaces(SLSMainConnectionID(), (__bridge CFArrayRef) ns_source_space);
                SLSManagedDisplaySetCurrentSpace(SLSMainConnectionID(), dest_display, dest_space_id);
                set_ivar_value(display_space, "_currentSpace", [dest_space retain]);
                dock_update_visibility_for_space(dest_space);
                [ns_dest_space release];
                [ns_source_space release];
                focused = true;
            }
        }
    }

    CFRelease(dest_display);
    return focused;
}

static void do_window_scale(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    CGRect frame = {};
    SLSGetWindowBounds(SLSMainConnectionID(), wid, &frame);
    CGAffineTransform original_transform = CGAffineTransformMakeTranslation(-frame.origin.x, -frame.origin.y);

    CGAffineTransform current_transform;
    SLSGetWindowTransform(SLSMainConnectionID(), wid, &current_transform);

    if (CGAffineTransformEqualToTransform(current_transform, original_transform)) {
        float dx, dy, dw, dh;
        unpack(dx);
        unpack(dy);
        unpack(dw);
        unpack(dh);

        int target_width  = dw / 4;
        int target_height = target_width / (frame.size.width/frame.size.height);

        float x_scale = frame.size.width/target_width;
        float y_scale = frame.size.height/target_height;

        CGFloat transformed_x = -(dx+dw) + (frame.size.width * (1/x_scale));
        CGFloat transformed_y = -dy;

        CGAffineTransform scale = CGAffineTransformConcat(CGAffineTransformIdentity, CGAffineTransformMakeScale(x_scale, y_scale));
        CGAffineTransform transform = CGAffineTransformTranslate(scale, transformed_x, transformed_y);
        SLSSetWindowTransform(SLSMainConnectionID(), wid, transform);
    } else {
        SLSSetWindowTransform(SLSMainConnectionID(), wid, original_transform);
    }
}

static void do_window_move(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    int x, y;
    unpack(x);
    unpack(y);

    CGPoint point = CGPointMake(x, y);
    SLSMoveWindowWithGroup(SLSMainConnectionID(), wid, &point);

    NSArray *window_list = @[ @(wid) ];
    SLSReassociateWindowsSpacesByGeometry(SLSMainConnectionID(), (__bridge CFArrayRef) window_list);
    [window_list release];
}

static void do_window_opacity(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    float alpha;
    unpack(alpha);

    pthread_mutex_lock(&window_fade_lock);
    struct window_fade_context *context = table_find(&window_fade_table, &wid);

    if (context) {
        context->alpha = alpha;
        context->duration = 0.0f;
        __asm__ __volatile__ ("" ::: "memory");

        context->skip = true;
        pthread_mutex_unlock(&window_fade_lock);
    } else {
        SLSSetWindowAlpha(SLSMainConnectionID(), wid, alpha);
        pthread_mutex_unlock(&window_fade_lock);
    }
}

static void *window_fade_thread_proc(void *data)
{
entry:;
    struct window_fade_context *context = (struct window_fade_context *) data;
    context->skip  = false;

    float start_alpha;
    float end_alpha = context->alpha;
    SLSGetWindowAlpha(SLSMainConnectionID(), context->wid, &start_alpha);

    int frame_duration = 8;
    int total_duration = (int)(context->duration * 1000.0f);
    int frame_count = (int)(((float) total_duration / (float) frame_duration) + 1.0f);

    for (int frame_index = 1; frame_index <= frame_count; ++frame_index) {
        if (context->skip) goto entry;

        float t = (float) frame_index / (float) frame_count;
        if (t < 0.0f) t = 0.0f;
        if (t > 1.0f) t = 1.0f;

        float alpha = lerp(start_alpha, t, end_alpha);
        SLSSetWindowAlpha(SLSMainConnectionID(), context->wid, alpha);

        usleep(frame_duration*1000);
    }

    pthread_mutex_lock(&window_fade_lock);
    if (!context->skip) {
        table_remove(&window_fade_table, &context->wid);
        pthread_mutex_unlock(&window_fade_lock);
        free(context);
        return NULL;
    }
    pthread_mutex_unlock(&window_fade_lock);

    goto entry;
}

static void do_window_opacity_fade(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    float alpha, duration;
    unpack(alpha);
    unpack(duration);

    pthread_mutex_lock(&window_fade_lock);
    struct window_fade_context *context = table_find(&window_fade_table, &wid);

    if (context) {
        context->alpha = alpha;
        context->duration = duration;
        __asm__ __volatile__ ("" ::: "memory");

        context->skip = true;
        pthread_mutex_unlock(&window_fade_lock);
    } else {
        context = malloc(sizeof(struct window_fade_context));
        context->wid = wid;
        context->alpha = alpha;
        context->duration = duration;
        context->skip = false;
        __asm__ __volatile__ ("" ::: "memory");

        table_add(&window_fade_table, &wid, context);
        pthread_mutex_unlock(&window_fade_lock);
        pthread_create(&context->thread, NULL, &window_fade_thread_proc, context);
        pthread_detach(context->thread);
    }
}

static void do_window_layer(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    int layer;
    unpack(layer);

    SLSSetWindowSubLevel(SLSMainConnectionID(), wid, CGWindowLevelForKey(layer));
}

static void do_window_sticky(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    bool value;
    unpack(value);

    uint64_t tags = (1 << 11);
    if (value == 1) {
        SLSSetWindowTags(SLSMainConnectionID(), wid, &tags, 64);
    } else {
        SLSClearWindowTags(SLSMainConnectionID(), wid, &tags, 64);
    }
}

typedef void (*focus_window_call)(ProcessSerialNumber psn, uint32_t wid);
static void do_window_focus(char *message)
{
    if (set_front_window_fp == 0) return;

    int window_connection;
    ProcessSerialNumber window_psn;

    uint32_t wid;
    unpack(wid);

    SLSGetWindowOwner(SLSMainConnectionID(), wid, &window_connection);
    SLSGetConnectionPSN(SLSMainConnectionID(), &window_psn);

    ((focus_window_call) set_front_window_fp)(window_psn, wid);
}

static void do_window_shadow(char *message)
{
    uint32_t wid;
    unpack(wid);
    if (!wid) return;

    bool value;
    unpack(value);

    uint64_t tags = (1 << 3);
    if (value == 1) {
        SLSClearWindowTags(SLSMainConnectionID(), wid, &tags, 64);
    } else {
        SLSSetWindowTags(SLSMainConnectionID(), wid, &tags, 64);
    }
}

static void do_window_swap_proxy_in(char *message)
{
    int count = 0;
    unpack(count);
    if (!count) return;

    CFTypeRef transaction = SLSTransactionCreate(SLSMainConnectionID());
    for (int i = 0; i < count; ++i) {
        uint32_t wid;
        unpack(wid);
        if (!wid) continue;

        uint32_t proxy_wid;
        unpack(proxy_wid);

        SLSTransactionOrderWindowGroup(transaction, proxy_wid, 1, wid);
        SLSTransactionSetWindowSystemAlpha(transaction, wid, 0);
    }
    SLSTransactionCommit(transaction, 0);
    CFRelease(transaction);
}

static void do_window_swap_proxy_out(char *message)
{
    int count = 0;
    unpack(count);
    if (!count) return;

    CFTypeRef transaction = SLSTransactionCreate(SLSMainConnectionID());
    for (int i = 0; i < count; ++i) {
        uint32_t wid;
        unpack(wid);
        if (!wid) continue;

        uint32_t proxy_wid;
        unpack(proxy_wid);

        SLSTransactionSetWindowSystemAlpha(transaction, wid, 1.0f);
        SLSTransactionOrderWindowGroup(transaction, proxy_wid, 0, wid);
    }
    SLSTransactionCommit(transaction, 0);
    CFRelease(transaction);
}

static void do_window_order(char *message)
{
    uint32_t a_wid;
    unpack(a_wid);
    if (!a_wid) return;

    int order;
    unpack(order);

    uint32_t b_wid;
    unpack(b_wid);

    SLSOrderWindow(SLSMainConnectionID(), a_wid, order, b_wid);
}

static void do_window_order_in(char *message)
{
    int count = 0;
    unpack(count);
    if (!count) return;

    CFTypeRef transaction = SLSTransactionCreate(SLSMainConnectionID());
    for (int i = 0; i < count; ++i) {
        uint32_t wid;
        unpack(wid);
        if (!wid) continue;

        SLSTransactionOrderWindowGroup(transaction, wid, 1, 0);
    }
    SLSTransactionCommit(transaction, 0);
    CFRelease(transaction);
}

static inline CFArrayRef cfarray_of_cfnumbers(void *values, size_t size, int count, CFNumberType type)
{
    CFNumberRef temp[count];

    for (int i = 0; i < count; ++i) {
        temp[i] = CFNumberCreate(NULL, type, ((char *)values) + (size * i));
    }

    CFArrayRef result = CFArrayCreate(NULL, (const void **)temp, count, &kCFTypeArrayCallBacks);

    for (int i = 0; i < count; ++i) {
        CFRelease(temp[i]);
    }

    return result;
}

static void do_window_list_move_to_space(char *message)
{
    uint64_t sid;
    unpack(sid);

    int count = 0;
    unpack(count);

    CFArrayRef window_list_ref = cfarray_of_cfnumbers((uint32_t*)message, sizeof(uint32_t), count, kCFNumberSInt32Type);
    SLSMoveWindowsToManagedSpace(SLSMainConnectionID(), window_list_ref, sid);
    CFRelease(window_list_ref);
}

static void do_window_move_to_space(char *message)
{
    uint64_t sid;
    unpack(sid);

    uint32_t wid;
    unpack(wid);

    CFArrayRef window_list_ref = cfarray_of_cfnumbers(&wid, sizeof(uint32_t), 1, kCFNumberSInt32Type);
    SLSMoveWindowsToManagedSpace(SLSMainConnectionID(), window_list_ref, sid);
    CFRelease(window_list_ref);
}

// [f64 duration seconds][f64 x1][f64 y1][f64 x2][f64 y2]. A positive duration
// installs the hook (if this Dock has the routine) and sets the curve; zero
// restores Dock's own animation. Applied on the main queue, where the
// animation timer runs, so the timing never changes under a running step.
static void do_space_switch_animation(char *message)
{
    double duration;
    unpack(duration);

    // A struct rather than an array: a block cannot capture an array.
    struct { double points[4]; } curve;
    unpack(curve);

#if __arm64__
    if (!space_step_addr) return;

    dispatch_sync(dispatch_get_main_queue(), ^{
        space_step_duration = duration > 0.0 ? duration : 0.0;
        memcpy(space_step_bezier, curve.points, sizeof(space_step_bezier));
        space_step_anim.active = false;
        space_step_set_hooked(duration > 0.0);
    });
#endif
}

static void do_handshake(int sockfd)
{
    uint32_t attrib = 0;

    if (dock_spaces != nil)                attrib |= OSAX_ATTRIB_DOCK_SPACES;
    if (dp_desktop_picture_manager != nil) attrib |= OSAX_ATTRIB_DPPM;
    if (add_space_fp)                      attrib |= OSAX_ATTRIB_ADD_SPACE;
    if (remove_space_fp)                   attrib |= OSAX_ATTRIB_REM_SPACE;
    if (move_space_fp)                     attrib |= OSAX_ATTRIB_MOV_SPACE;
    if (set_front_window_fp)               attrib |= OSAX_ATTRIB_SET_WINDOW;
    if (animation_time_addr)               attrib |= OSAX_ATTRIB_ANIM_TIME;
    if (space_step_addr)                   attrib |= OSAX_ATTRIB_SPACE_STEP;

    char bytes[BUFSIZ] = {};
    int version_length = strlen(OSAX_VERSION);
    int attrib_length = sizeof(uint32_t);
    int bytes_length = version_length + 1 + attrib_length;

    memcpy(bytes, OSAX_VERSION, version_length);
    memcpy(bytes + version_length + 1, &attrib, attrib_length);
    bytes[version_length] = '\0';
    bytes[bytes_length] = '\n';

    send(sockfd, bytes, bytes_length+1, 0);
}

static void handle_message(int sockfd, char *message)
{
    enum sa_opcode op = *message++;
    switch (op) {
    case SA_OPCODE_HANDSHAKE: {
        do_handshake(sockfd);
    } break;
    case SA_OPCODE_SPACE_FOCUS: {
        char focused = do_space_focus(message);
        send(sockfd, &focused, 1, 0);
    } break;
    case SA_OPCODE_SPACE_CREATE: {
        do_space_create(message);
    } break;
    case SA_OPCODE_SPACE_DESTROY: {
        do_space_destroy(message);
    } break;
    case SA_OPCODE_SPACE_MOVE: {
        do_space_move(message);
    } break;
    case SA_OPCODE_WINDOW_MOVE: {
        do_window_move(message);
    } break;
    case SA_OPCODE_WINDOW_OPACITY: {
        do_window_opacity(message);
    } break;
    case SA_OPCODE_WINDOW_OPACITY_FADE: {
        do_window_opacity_fade(message);
    } break;
    case SA_OPCODE_WINDOW_LAYER: {
        do_window_layer(message);
    } break;
    case SA_OPCODE_WINDOW_STICKY: {
        do_window_sticky(message);
    } break;
    case SA_OPCODE_WINDOW_SHADOW: {
        do_window_shadow(message);
    } break;
    case SA_OPCODE_WINDOW_FOCUS: {
        do_window_focus(message);
    } break;
    case SA_OPCODE_WINDOW_SCALE: {
        do_window_scale(message);
    } break;
    case SA_OPCODE_WINDOW_SWAP_PROXY_IN: {
        do_window_swap_proxy_in(message);
    } break;
    case SA_OPCODE_WINDOW_SWAP_PROXY_OUT: {
        do_window_swap_proxy_out(message);
    } break;
    case SA_OPCODE_WINDOW_ORDER: {
        do_window_order(message);
    } break;
    case SA_OPCODE_WINDOW_ORDER_IN: {
        do_window_order_in(message);
    } break;
    case SA_OPCODE_SPACE_SWITCH_ANIMATION: {
        do_space_switch_animation(message);
    } break;
    case SA_OPCODE_WINDOW_LIST_TO_SPACE: {
        do_window_list_move_to_space(message);
    } break;
    case SA_OPCODE_WINDOW_TO_SPACE: {
        do_window_move_to_space(message);
    } break;
    }
}

static inline bool read_message(int sockfd, char *message)
{
    int bytes_read    = 0;
    int bytes_to_read = 0;

    if (read(sockfd, &bytes_to_read, sizeof(int16_t)) == sizeof(int16_t)) {
        if (bytes_to_read >= SA_SOCKET_BUFF_LEN) return false;
        if (bytes_to_read <= 0)                  return false;

        do {
            int cur_read = read(sockfd, message+bytes_read, bytes_to_read-bytes_read);
            if (cur_read <= 0) break;

            bytes_read += cur_read;
        } while (bytes_read < bytes_to_read);

        return bytes_read == bytes_to_read;
    }

    return false;
}

static void *handle_connection(void *unused)
{
    for (;;) {
        int sockfd = accept(daemon_sockfd, NULL, 0);
        if (sockfd == -1) continue;

        char message[SA_SOCKET_BUFF_LEN];
        if (read_message(sockfd, message)) {
            handle_message(sockfd, message);
        }

        shutdown(sockfd, SHUT_RDWR);
        close(sockfd);
    }

    return NULL;
}

static TABLE_HASH_FUNC(hash_wid)
{
    return *(uint32_t *) key;
}

static TABLE_COMPARE_FUNC(compare_wid)
{
    return *(uint32_t *) key_a == *(uint32_t *) key_b;
}

static bool start_daemon(char *socket_path)
{
    struct sockaddr_un socket_address;
    socket_address.sun_family = AF_UNIX;
    snprintf(socket_address.sun_path, sizeof(socket_address.sun_path), "%s", socket_path);
    unlink(socket_path);

    if ((daemon_sockfd = socket(AF_UNIX, SOCK_STREAM, 0)) == -1) {
        return false;
    }

    if (bind(daemon_sockfd, (struct sockaddr *) &socket_address, sizeof(socket_address)) == -1) {
        return false;
    }

    if (chmod(socket_path, 0600) != 0) {
        return false;
    }

    if (listen(daemon_sockfd, SOMAXCONN) == -1) {
        return false;
    }

    init_instances();
    pthread_mutex_init(&window_fade_lock, NULL);
    table_init(&window_fade_table, 150, hash_wid, compare_wid);
    pthread_create(&daemon_thread, NULL, &handle_connection, NULL);

    return true;
}

__attribute__((constructor))
void load_payload(void)
{
    NSLog(@"[rift-sa] loaded payload..");

    const char *user = getenv("USER");
    if (!user) {
        NSLog(@"[rift-sa] could not get 'env USER'! abort..");
        return;
    }

    char socket_file[255];
    snprintf(socket_file, sizeof(socket_file), SA_SOCKET_PATH_FMT, user);

    if (start_daemon(socket_file)) {
        NSLog(@"[rift-sa] now listening..");
    } else {
        NSLog(@"[rift-sa] failed to spawn thread..");
    }
}
