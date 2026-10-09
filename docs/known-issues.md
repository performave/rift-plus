# Known issues

Problems reported and not yet resolved, with what is known so far. One `##`
section per issue; when one is fixed, delete its section and let the changelog
say so.

## Mission Control desktop drag between displays feels imprecise

Reported 2026-09-29. Dragging a desktop thumbnail from one display's Spaces bar
to the other's in Mission Control feels off: it cannot be placed precisely and
seems to snap to the other display. Whether rift is involved at all is not yet
known.

The flight recorder for two such drags that day shows rift staying out of the
way. Its event tap (`src/actor/input.rs`) is an active tap
(`CGEventTapOptions::Default`) that sees `LeftMouseDragged`, and it passed every
event through unconsumed, each callback under 1 ms; only the initial mouse-down
took longer, at 2.4 ms. No warps, window writes, space switches or
scripting-addition calls happened during the drag, and `MouseDragged` events
arrived at a steady ~60 Hz with no stalls.

rift can touch Dock during such a drag in only two ways: through that event
tap, and through the scripting addition living inside Dock's process. The
addition's `space_switch_animation` hook replaces only the spring that follows
a swipe's release, so it is an unlikely cause of how the drag itself feels.

**Next step:** A/B with rift fully out of the picture. `rift service stop`, then
`killall Dock` to drop the addition out of Dock, and try the drag. Afterwards
`rift service start`, and `sudo rift sa load` if `rift sa status` says the
addition is not loaded. If the drag is only off with rift running, bisect in the
[VM harness](vm-harness.md), which can hotplug a second virtual display: tap on
and off, addition on and off, measuring the thumbnail's offset from the cursor
across frames.
