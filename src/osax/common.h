#ifndef RIFT_SA_COMMON_H
#define RIFT_SA_COMMON_H

//
// Shared between the payload that runs inside Dock and rift's own client.
// Anything changed here must be changed in `src/sys/scripting_addition.rs`
// and `src/sys/osax.rs` to match -- the two sides are wire-compatible by
// convention, not by a generated header.
//
// The socket name and bundle identifiers are rift's own, so rift's payload
// and yabai's can be injected into the same Dock without either noticing the
// other: two dylibs, two sockets, no shared symbols.
//

#define SA_SOCKET_PATH_FMT "/tmp/rift-sa_%s.socket"
#define SA_SOCKET_BUFF_LEN 0x1000

//
// rift's own numbering, not yabai's. Bump it whenever payload.m changes in a
// way a running Dock would have to pick up: `rift sa load` compares this
// against what the payload answers with and reinstalls when they differ.
//
#define OSAX_VERSION                "1.3.5"

#define OSAX_ATTRIB_DOCK_SPACES     0x01
#define OSAX_ATTRIB_DPPM            0x02
#define OSAX_ATTRIB_ADD_SPACE       0x04
#define OSAX_ATTRIB_REM_SPACE       0x08
#define OSAX_ATTRIB_MOV_SPACE       0x10
#define OSAX_ATTRIB_SET_WINDOW      0x20
#define OSAX_ATTRIB_ANIM_TIME       0x40
// rift's own: the routine Dock steps the trackpad space switch with, hooked
// on SA_OPCODE_SPACE_SWITCH_ANIMATION.
#define OSAX_ATTRIB_SPACE_STEP      0x80

#define OSAX_ATTRIB_ALL             (OSAX_ATTRIB_DOCK_SPACES | \
                                     OSAX_ATTRIB_DPPM | \
                                     OSAX_ATTRIB_ADD_SPACE | \
                                     OSAX_ATTRIB_REM_SPACE | \
                                     OSAX_ATTRIB_MOV_SPACE | \
                                     OSAX_ATTRIB_SET_WINDOW | \
                                     OSAX_ATTRIB_ANIM_TIME | \
                                     OSAX_ATTRIB_SPACE_STEP)

enum sa_opcode
{
    SA_OPCODE_HANDSHAKE             = 0x01,
    SA_OPCODE_SPACE_FOCUS           = 0x02,
    SA_OPCODE_SPACE_CREATE          = 0x03,
    SA_OPCODE_SPACE_DESTROY         = 0x04,
    SA_OPCODE_SPACE_MOVE            = 0x05,
    SA_OPCODE_WINDOW_MOVE           = 0x06,
    SA_OPCODE_WINDOW_OPACITY        = 0x07,
    SA_OPCODE_WINDOW_OPACITY_FADE   = 0x08,
    SA_OPCODE_WINDOW_LAYER          = 0x09,
    SA_OPCODE_WINDOW_STICKY         = 0x0A,
    SA_OPCODE_WINDOW_SHADOW         = 0x0B,
    SA_OPCODE_WINDOW_FOCUS          = 0x0C,
    SA_OPCODE_WINDOW_SCALE          = 0x0D,
    SA_OPCODE_WINDOW_SWAP_PROXY_IN  = 0x0E,
    SA_OPCODE_WINDOW_SWAP_PROXY_OUT = 0x0F,
    SA_OPCODE_WINDOW_ORDER          = 0x10,
    SA_OPCODE_WINDOW_ORDER_IN       = 0x11,
    SA_OPCODE_WINDOW_LIST_TO_SPACE  = 0x12,
    SA_OPCODE_WINDOW_TO_SPACE       = 0x13,
    // rift's own, past yabai's numbering: [f64 duration seconds][f64 x1, y1,
    // x2, y2]. A duration of zero restores Dock's animation.
    SA_OPCODE_SPACE_SWITCH_ANIMATION = 0x14,
};

#endif
