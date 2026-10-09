# Third-party docks

macOS lets no app reserve part of the screen. The usable area it reports,
`visibleFrame`, makes room for Apple's Dock and the menu bar and nothing else,
so a dock drawn by another app is just a window, and every tiling window
manager on macOS lays tiles out underneath it. rift keeps the space clear
itself for the apps you name.

```toml
[settings.layout]
external_docks = ["com.ejbills.DockDoorPro"]
```

The value is a list of bundle ids (`osascript -e 'id of app "Name"'` prints
one). DockDoor Pro is listed by default. Set `external_docks = []` to turn the
feature off. A change takes effect when the config reloads.

## How it works

The code is `src/sys/external_dock.rs` (finding and measuring a dock) and
`src/actor/external_dock.rs` (deciding when to measure, on a thread of its own).

**Where the dock is** comes from the window server: an on-screen window of
that app, above the normal window level, lying flush against one edge of a
display and running at least half its length, and no thicker than half the
display. A dock at the top may sit under the menu bar. Full-screen overlays and
floating panels fail one test or another.

**How thick it is** comes from Accessibility, because the window is no guide.
DockDoor Pro's is 221–254pt wide around a bar 42pt wide, set 4pt in from the
edge; the rest is transparent room for magnified icons and hover labels. rift
reads the frames of the window's elements, clips them to the window, and takes
the element that runs furthest along the edge, which is the bar. A hover label
or magnified icon sticks out sideways but is short, so hovering over the dock
does not move tiles. The reservation is measured from the display's edge, so
for DockDoor Pro it is 46pt, and your outer gap still applies on top.

When the window is cut to the bar (its contents reach within 16pt of its far
edge), or it exposes no elements at all, the window itself is taken as the
bar.

**When it is hidden.** If the dock's window is ordered out, moves off its edge,
or has content none of which is inside the window any more, the space comes
back.

**When rift looks again.** The dock app's own Accessibility notifications
(layout, moves, windows coming and going) trigger a measurement once they have
been quiet for 200ms. Some changes produce no notification, so the window
server is also read every two seconds while a listed app is running: one
window-list query, no requests to the dock app. A new answer has to hold for
700ms before tiles move for it, so a drawer sliding out of the dock's edge, or
a dock revealed for a moment, does not rearrange the display. Quitting the
app, or changing the config, applies at once.

## Limits

- **Auto-hide.** A dock that auto-hides is reserved while it is showing, if it
  stays up longer than the 700ms settle time. If you use your dock's
  auto-hide, take it out of `external_docks` and leave the space to it.
- **A dock that only fades out**, with its window and elements left where they
  were, cannot be told apart from one that is showing. rift keeps the space.
- **One dock per app per display.** Where an app shows several qualifying
  windows on one display, the thinnest is taken; the others are taken to be
  drawers and panels, which open over your windows by design.
- **Shortcuts.** rift's key tap sees keys first, so a shortcut bound both in
  rift and in the dock app (DockDoor Pro's "toggle dock hidden" against a
  focus key, say) only ever reaches rift. Give one of them another key.
- **The dock app's own overlap avoidance.** DockDoor Pro's *Avoid Dock
  Overlap* resizes windows itself and would fight rift's tiling; leave it off.
