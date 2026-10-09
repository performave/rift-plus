//! Docks drawn by other apps: screen space macOS does not know is taken.
//!
//! macOS gives an app no way to reserve part of a display. `visibleFrame`
//! accounts for Apple's Dock and the menu bar and nothing else, so a dock drawn
//! by another app (DockDoor Pro, say) is just a window, and tiles run
//! underneath it. rift reserves the space itself for the apps named in
//! `settings.layout.external_docks`.
//!
//! Where the dock is comes from the window server: an on-screen window of that
//! app, above the normal level, lying flush against one edge of a display and
//! running most of its length. How thick it is comes from Accessibility,
//! because the window is no guide. DockDoor Pro's is 254pt wide around a bar
//! 42pt wide; the rest is transparent room for magnified icons and hover
//! labels. The bar is the element that runs furthest along the edge. Labels and
//! magnified icons stick out sideways from it but are short, so measuring the
//! longest element, rather than everything, keeps a hover from moving tiles.

use std::sync::RwLock;

use objc2_core_foundation::CGRect;
use tracing::debug;

use crate::sys::app::pid_t;
use crate::sys::axuielement::{AXUIElement, Error as AxError};
use crate::sys::geometry::CGRectExt;
use crate::sys::window_server;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Space a dock takes from one edge of a display, measured from the display's
/// own edge, so a dock at the top includes the menu bar it sits beneath.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reservation {
    /// The display's bounds, as `CGDisplayBounds` gives them.
    pub display: CGRect,
    pub edge: Edge,
    pub thickness: f64,
}

/// How far a window may sit from an edge and still count as against it.
const FLUSH_TOLERANCE: f64 = 2.0;
/// A dock runs at least this much of its edge's length...
const MIN_EDGE_COVERAGE: f64 = 0.5;
/// ...and is no thicker than this much of the display, which rules out
/// full-screen overlays, which touch every edge.
const MAX_THICKNESS_FRACTION: f64 = 0.5;
/// A window this close to its contents' reach is the bar itself, padding
/// included, rather than a transparent canvas around it.
const TIGHT_WINDOW_SLACK: f64 = 16.0;
/// How deep below the window to look for the bar. DockDoor Pro's sits at
/// depth two, under one hosting view the size of the window.
const MAX_DEPTH: usize = 3;
/// A bound on Accessibility requests per measurement.
const MAX_ELEMENTS: usize = 400;

static RESERVATIONS: RwLock<Vec<Reservation>> = RwLock::new(Vec::new());

/// The reservations in force. Read on every screen refresh.
pub fn reservations() -> Vec<Reservation> {
    RESERVATIONS.read().map(|r| r.clone()).unwrap_or_default()
}

pub fn set_reservations(reservations: Vec<Reservation>) {
    if let Ok(mut current) = RESERVATIONS.write() {
        *current = reservations;
    }
}

/// Shrinks `frame`, the usable part of `display`, clear of every reservation
/// on that display. Each one is a limit measured from the display's edge, not
/// an amount taken off, so a dock already cleared by Apple's Dock or the menu
/// bar costs nothing more.
pub fn apply(frame: CGRect, display: CGRect, reservations: &[Reservation]) -> CGRect {
    let mut frame = frame;
    for reservation in reservations.iter().filter(|r| same_rect(r.display, display)) {
        let thickness = reservation.thickness;
        match reservation.edge {
            Edge::Left => {
                let limit = display.origin.x + thickness;
                let cut = limit - frame.origin.x;
                if cut > 0.0 {
                    frame.origin.x = limit;
                    frame.size.width = (frame.size.width - cut).max(0.0);
                }
            }
            Edge::Right => {
                let limit = display.origin.x + display.size.width - thickness;
                frame.size.width = frame.size.width.min(limit - frame.origin.x).max(0.0);
            }
            Edge::Top => {
                let limit = display.origin.y + thickness;
                let cut = limit - frame.origin.y;
                if cut > 0.0 {
                    frame.origin.y = limit;
                    frame.size.height = (frame.size.height - cut).max(0.0);
                }
            }
            Edge::Bottom => {
                let limit = display.origin.y + display.size.height - thickness;
                frame.size.height = frame.size.height.min(limit - frame.origin.y).max(0.0);
            }
        }
    }
    frame
}

/// The edge of `display` that `window` is docked against, if it is shaped
/// like a dock there. A dock at the top sits under the menu bar, so its edge
/// counts as starting `menu_bar` below the display's.
pub fn docked_edge(window: CGRect, display: CGRect, menu_bar: f64) -> Option<Edge> {
    let on_display = window.origin.x >= display.origin.x - FLUSH_TOLERANCE
        && window.origin.y >= display.origin.y - FLUSH_TOLERANCE
        && max_x(window) <= max_x(display) + FLUSH_TOLERANCE
        && max_y(window) <= max_y(display) + FLUSH_TOLERANCE;
    if !on_display {
        return None;
    }
    [Edge::Left, Edge::Right, Edge::Bottom, Edge::Top].into_iter().find(|&edge| {
        let allowance = if edge == Edge::Top { menu_bar } else { 0.0 };
        let (length, edge_length) = match edge {
            Edge::Left | Edge::Right => (window.size.height, display.size.height),
            Edge::Top | Edge::Bottom => (window.size.width, display.size.width),
        };
        let (thickness, span) = match edge {
            Edge::Left | Edge::Right => (window.size.width, display.size.width),
            Edge::Top | Edge::Bottom => (window.size.height, display.size.height),
        };
        gap(edge, window, display) <= allowance + FLUSH_TOLERANCE
            && length >= MIN_EDGE_COVERAGE * edge_length
            && thickness <= MAX_THICKNESS_FRACTION * span
    })
}

/// How far into `display` the dock in `window` reaches, given the frames of
/// the window's accessibility elements.
///
/// Returns `None` when the window holds content but none of it is inside the
/// window: the dock has slid out of its own window to hide.
pub fn bar_thickness(
    edge: Edge,
    display: CGRect,
    window: CGRect,
    elements: &[CGRect],
) -> Option<f64> {
    let window_reach = reach(edge, window, display);
    // (length along the edge, reach) of the longest element so far.
    let mut bar: Option<(f64, f64)> = None;
    let mut content = false;
    for element in elements {
        // A view the size of the window is a canvas, not the bar. It stays
        // put when the dock slides out of the window to hide.
        if same_rect(*element, window) {
            continue;
        }
        content = true;
        let clipped = element.intersection(&window);
        if clipped.size.width <= 0.0 || clipped.size.height <= 0.0 {
            continue;
        }
        let length = match edge {
            Edge::Left | Edge::Right => clipped.size.height,
            Edge::Top | Edge::Bottom => clipped.size.width,
        };
        let element_reach = reach(edge, clipped, display);
        bar = match bar {
            Some((best, best_reach)) if length < best - 0.5 => Some((best, best_reach)),
            Some((best, best_reach)) if length <= best + 0.5 => {
                Some((best, best_reach.max(element_reach)))
            }
            _ => Some((length, element_reach)),
        };
    }
    match bar {
        Some((_, reach)) if window_reach - reach > TIGHT_WINDOW_SLACK => Some(reach),
        Some(_) => Some(window_reach),
        // Content, none of it inside the window: hidden.
        None if content => None,
        // Nothing to measure by, or only canvases: the window is the bar.
        None => Some(window_reach),
    }
}

/// Where `pid`'s dock windows are, as (window, display, edge). Answered by the
/// window server alone, so it is cheap enough to poll.
pub fn docked_windows(pid: pid_t) -> Vec<(CGRect, CGRect, Edge)> {
    let windows = window_server::onscreen_raised_windows(pid);
    if windows.is_empty() {
        return Vec::new();
    }
    let displays = crate::sys::screen::active_display_bounds();
    windows
        .into_iter()
        .filter_map(|window| {
            displays.iter().find_map(|&(did, display)| {
                let menu_bar = crate::sys::screen::menu_bar_height(did);
                docked_edge(window, display, menu_bar).map(|edge| (window, display, edge))
            })
        })
        .collect()
}

/// Measures every dock `pid` shows, one per display at most. Where an app
/// shows several windows that qualify on one display, the thinnest is taken:
/// a dock app's other edge-hugging windows are drawers and panels that open
/// over the work, which is what they are for.
///
/// An Accessibility failure is an error, not an empty answer, so the caller
/// can keep what it had rather than drop a reservation because the app was
/// busy.
pub fn measure(pid: pid_t) -> Result<Vec<Reservation>, AxError> {
    let docked = docked_windows(pid);
    if docked.is_empty() {
        return Ok(Vec::new());
    }
    let ax_windows = AXUIElement::application(pid).windows()?;
    let mut out: Vec<Reservation> = Vec::new();
    for (window, display, edge) in docked {
        let Some(ax_window) = ax_windows
            .iter()
            .find(|w| w.frame().is_ok_and(|frame| same_rect(frame, window)))
        else {
            debug!(
                ?window,
                pid, "dock window has no accessibility window; not reserving"
            );
            continue;
        };
        let elements = element_frames(ax_window)?;
        let Some(thickness) = bar_thickness(edge, display, window, &elements) else {
            continue;
        };
        // Whole points, so sub-pixel layout jitter is not a change.
        let thickness = thickness.ceil();
        let reservation = Reservation { display, edge, thickness };
        match out.iter_mut().find(|r| same_rect(r.display, display)) {
            Some(existing) if thickness < existing.thickness => *existing = reservation,
            Some(_) => {}
            None => out.push(reservation),
        }
    }
    Ok(out)
}

fn element_frames(window: &AXUIElement) -> Result<Vec<CGRect>, AxError> {
    let mut frames = Vec::new();
    let mut level = window.children()?;
    for depth in 1..=MAX_DEPTH {
        let mut next = Vec::new();
        for element in level {
            if frames.len() >= MAX_ELEMENTS {
                return Ok(frames);
            }
            if let Ok(frame) = element.frame() {
                frames.push(frame);
            }
            if depth < MAX_DEPTH {
                next.extend(element.children().unwrap_or_default());
            }
        }
        level = next;
    }
    Ok(frames)
}

fn gap(edge: Edge, window: CGRect, display: CGRect) -> f64 {
    match edge {
        Edge::Left => window.origin.x - display.origin.x,
        Edge::Right => max_x(display) - max_x(window),
        Edge::Top => window.origin.y - display.origin.y,
        Edge::Bottom => max_y(display) - max_y(window),
    }
}

fn reach(edge: Edge, rect: CGRect, display: CGRect) -> f64 {
    match edge {
        Edge::Left => max_x(rect) - display.origin.x,
        Edge::Right => max_x(display) - rect.origin.x,
        Edge::Top => max_y(rect) - display.origin.y,
        Edge::Bottom => max_y(display) - rect.origin.y,
    }
}

fn max_x(rect: CGRect) -> f64 { rect.origin.x + rect.size.width }

fn max_y(rect: CGRect) -> f64 { rect.origin.y + rect.size.height }

fn same_rect(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() <= 1.0
        && (a.origin.y - b.origin.y).abs() <= 1.0
        && (a.size.width - b.size.width).abs() <= 1.0
        && (a.size.height - b.size.height).abs() <= 1.0
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    // Measured from DockDoor Pro 1.6.0 on a 1512x982 built-in display, docked
    // left, 32pt icons, floating style.
    fn display() -> CGRect { rect(0.0, 0.0, 1512.0, 982.0) }
    fn dock_window() -> CGRect { rect(0.0, 33.0, 254.0, 949.0) }

    fn dockdoor_elements() -> Vec<CGRect> {
        let mut elements = vec![
            // The hosting view, the size of the window.
            rect(0.0, 33.0, 254.0, 949.0),
            // The bar.
            rect(4.0, 169.5, 42.0, 675.5),
            // The "Home" label pokes out past the screen edge.
            rect(-11.0, 185.3, 52.0, 17.5),
        ];
        for i in 0..16 {
            let y = 207.0 + 36.0 * i as f64;
            elements.push(rect(9.0, y, 32.0, 32.0));
            elements.push(rect(11.0, y - 1.0, 34.1, 34.1));
        }
        elements
    }

    #[test]
    fn dockdoor_window_is_docked_left() {
        assert_eq!(docked_edge(dock_window(), display(), 33.0), Some(Edge::Left));
    }

    #[test]
    fn measures_the_bar_not_the_window() {
        let thickness =
            bar_thickness(Edge::Left, display(), dock_window(), &dockdoor_elements()).unwrap();
        assert_eq!(thickness, 46.0);
    }

    #[test]
    fn a_hover_label_does_not_widen_the_bar() {
        let mut elements = dockdoor_elements();
        // A magnified icon and its label, reaching 134.8pt in while hovered.
        elements.push(rect(9.0, 680.0, 64.0, 64.0));
        elements.push(rect(50.0, 700.0, 84.8, 20.0));
        let thickness = bar_thickness(Edge::Left, display(), dock_window(), &elements).unwrap();
        assert_eq!(thickness, 46.0);
    }

    #[test]
    fn a_dock_slid_out_of_its_window_is_hidden() {
        // Everything but the window-sized hosting view moves off-screen.
        let hidden: Vec<CGRect> = dockdoor_elements()
            .into_iter()
            .map(|r| {
                if same_rect(r, dock_window()) {
                    r
                } else {
                    rect(r.origin.x - 300.0, r.origin.y, r.size.width, r.size.height)
                }
            })
            .collect();
        assert_eq!(
            bar_thickness(Edge::Left, display(), dock_window(), &hidden),
            None
        );
    }

    #[test]
    fn a_window_holding_only_a_canvas_reserves_the_window() {
        let window = rect(0.0, 100.0, 48.0, 700.0);
        assert_eq!(
            bar_thickness(Edge::Left, display(), window, &[window]),
            Some(48.0)
        );
    }

    #[test]
    fn a_window_cut_to_the_bar_reserves_the_window() {
        let window = rect(0.0, 100.0, 48.0, 700.0);
        // No bar element, only icons, which stop short of the window's edge.
        let icons: Vec<CGRect> =
            (0..10).map(|i| rect(8.0, 110.0 + 40.0 * i as f64, 32.0, 32.0)).collect();
        assert_eq!(bar_thickness(Edge::Left, display(), window, &icons), Some(48.0));
    }

    #[test]
    fn a_window_without_elements_reserves_the_window() {
        let window = rect(0.0, 900.0, 1512.0, 82.0);
        assert_eq!(bar_thickness(Edge::Bottom, display(), window, &[]), Some(82.0));
    }

    #[test]
    fn full_screen_overlays_and_floating_panels_are_not_docks() {
        assert_eq!(docked_edge(display(), display(), 33.0), None);
        // DockDoor Pro's settings window, mid-screen.
        assert_eq!(
            docked_edge(rect(236.0, 168.0, 1040.0, 680.0), display(), 33.0),
            None
        );
        // Against the left edge but short.
        assert_eq!(docked_edge(rect(0.0, 300.0, 60.0, 200.0), display(), 33.0), None);
        // On another display.
        assert_eq!(
            docked_edge(rect(1512.0, 33.0, 60.0, 949.0), display(), 33.0),
            None
        );
    }

    #[test]
    fn edges_are_found_on_every_side() {
        assert_eq!(
            docked_edge(rect(1452.0, 33.0, 60.0, 949.0), display(), 33.0),
            Some(Edge::Right)
        );
        assert_eq!(
            docked_edge(rect(100.0, 900.0, 1300.0, 82.0), display(), 33.0),
            Some(Edge::Bottom)
        );
        // Under the menu bar rather than at the display's edge.
        assert_eq!(
            docked_edge(rect(100.0, 33.0, 1300.0, 60.0), display(), 33.0),
            Some(Edge::Top)
        );
        assert_eq!(
            docked_edge(rect(100.0, 33.0, 1300.0, 60.0), display(), 0.0),
            None
        );
    }

    #[test]
    fn apply_clears_each_edge() {
        let frame = rect(0.0, 34.0, 1512.0, 948.0);
        let reserve = |edge, thickness| {
            [Reservation {
                display: display(),
                edge,
                thickness,
            }]
        };
        assert_eq!(
            apply(frame, display(), &reserve(Edge::Left, 46.0)),
            rect(46.0, 34.0, 1466.0, 948.0)
        );
        assert_eq!(
            apply(frame, display(), &reserve(Edge::Right, 46.0)),
            rect(0.0, 34.0, 1466.0, 948.0)
        );
        assert_eq!(
            apply(frame, display(), &reserve(Edge::Bottom, 82.0)),
            rect(0.0, 34.0, 1512.0, 866.0)
        );
        // Measured from the display's edge, so the menu bar is inside it.
        assert_eq!(
            apply(frame, display(), &reserve(Edge::Top, 93.0)),
            rect(0.0, 93.0, 1512.0, 889.0)
        );
    }

    #[test]
    fn apply_does_not_stack_on_space_already_cleared() {
        // Apple's Dock has already taken 70pt on the left; a 46pt dock there
        // sits within it.
        let frame = rect(70.0, 34.0, 1442.0, 948.0);
        let reservations = [Reservation {
            display: display(),
            edge: Edge::Left,
            thickness: 46.0,
        }];
        assert_eq!(apply(frame, display(), &reservations), frame);
    }

    #[test]
    fn apply_ignores_other_displays() {
        let frame = rect(0.0, 34.0, 1512.0, 948.0);
        let other = rect(1512.0, 0.0, 2560.0, 1440.0);
        let reservations = [Reservation {
            display: other,
            edge: Edge::Left,
            thickness: 46.0,
        }];
        assert_eq!(apply(frame, display(), &reservations), frame);
    }
}
